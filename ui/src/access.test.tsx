import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import { tokenStore } from './api/http';
import { browser } from './auth/sso';
import { mockFetch, renderApp } from './test/render';

const ADMIN_PERMS = { global: ['*'], spaces: {} };

interface Principal {
  name: string;
  role: 'viewer' | 'editor' | 'admin';
  kind: string;
  permissions: { global: string[]; spaces: Record<string, string[]> };
  groups?: string[];
}

function info(p: Principal) {
  return { name: 'FastMock', version: '0.1.0', storage: 'memory', uptimeSeconds: 5, stubsLoaded: 2, plugins: [], principal: p, authDisabled: false };
}

const SPACES = [
  { name: 'billing', pathPrefixes: [], hosts: [], permissions: ['stubs:read'] },
  { name: 'payments', description: 'Payment provider mocks', pathPrefixes: ['/payments/'], hosts: [], permissions: ['stubs:read', 'stubs:write'] },
];

const STUBS = [
  { id: '6f1c3b8e-0000-4000-8000-000000000001', name: 'pay', request: { method: 'GET', urlPath: '/payments/1' }, response: { status: 200 }, enabled: true, space: 'payments' },
  { id: '6f1c3b8e-0000-4000-8000-000000000002', name: 'bill', request: { method: 'GET', urlPath: '/bill' }, response: { status: 200 }, enabled: true, space: 'billing' },
];

interface Server {
  principal: Principal;
  oidc?: boolean;
  /** Authenticate with the session cookie (no Authorization header) instead of the bearer token `good`. */
  session?: boolean;
  spaces?: typeof SPACES;
  expireSession?: () => boolean;
}

function server(s: Server) {
  return mockFetch((url, init) => {
    const auth = init.headers.get('authorization');
    if (url.pathname === '/openapi.json') return { body: { paths: { '/__admin/tokens': { get: {} }, '/__admin/files': { get: {} } }, components: { schemas: {} } } };
    if (url.pathname === '/__admin/auth/config') return s.oidc === undefined ? { status: 404, body: { error: 'not found' } } : { body: { oidc: s.oidc, tokenLogin: true } };
    if (url.pathname === '/__admin/auth/logout') return { body: { loggedOut: true, logoutUrl: 'https://idp.example.com/logout?x=1' } };
    if (url.pathname.startsWith('/__admin')) {
      const ok = s.session ? !auth && !s.expireSession?.() : auth === 'Bearer good';
      if (!ok) return { status: 401, body: { error: 'unauthorized' } };
    }
    if (url.pathname === '/__admin/info') return { body: info(s.principal) };
    if (url.pathname === '/__admin/spaces') return { body: { spaces: s.spaces ?? [] } };
    if (url.pathname === '/__admin/stubs' && init.method === 'GET') {
      const space = url.searchParams.get('space');
      const stubs = STUBS.filter((x) => !space || x.space === space);
      return { body: { total: stubs.length, offset: 0, stubs } };
    }
    if (url.pathname.startsWith('/__admin/stubs/') && init.method === 'PATCH') return { body: STUBS[0] };
    return undefined;
  });
}

beforeEach(() => {
  tokenStore.set(null);
  tokenStore.setSession(false);
  window.sessionStorage.clear();
  window.localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

describe('login page', () => {
  it('offers token sign-in only when SSO is not configured', async () => {
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS } });
    renderApp(<AppRoutes />, '/login');
    expect(await screen.findByTestId('token-input')).toBeInTheDocument();
    expect(screen.getByTestId('login-submit')).toHaveTextContent('Sign in');
    expect(screen.queryByTestId('sso-login')).not.toBeInTheDocument();
  });

  it('starts SSO sign-in with the return path and keeps token sign-in', async () => {
    const assign = vi.spyOn(browser, 'assign').mockImplementation(() => undefined);
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, oidc: true });
    renderApp(<AppRoutes />, '/stubs?tag=a');
    await userEvent.click(await screen.findByTestId('sso-login'));
    expect(assign).toHaveBeenCalledWith('/__admin/auth/login?return=%2Fui%2Fstubs%3Ftag%3Da');
    expect(screen.getByTestId('token-input')).toBeInTheDocument();
  });

  it('shows the error the server put on the callback redirect', async () => {
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, oidc: true });
    renderApp(<AppRoutes />, '/?loginError=no_access');
    expect(await screen.findByTestId('login-error')).toHaveTextContent(/no access to this FastMock server/);
  });
});

describe('session mode', () => {
  const user: Principal = { name: 'alice', role: 'editor', kind: 'user', groups: ['payments-dev'], permissions: { global: ['info:read'], spaces: { payments: ['stubs:read', 'stubs:write'] } } };

  it('sends the CSRF header and cookies, never a bearer token, and shows the user', async () => {
    const calls = server({ principal: user, session: true, spaces: SPACES.slice(1) });
    renderApp(<AppRoutes />, '/stubs');
    const row = (await screen.findAllByTestId('stub-row'))[0];
    if (!row) throw new Error('no row');
    expect(screen.getByTestId('principal-name')).toHaveTextContent('alice');
    expect(screen.getByTestId('kind-badge')).toHaveTextContent('user');
    expect(screen.getByTestId('role-badge')).toHaveTextContent('payments');
    await userEvent.click(within(row).getByRole('switch'));
    await waitFor(() => expect(calls.some((c) => c.method === 'PATCH')).toBe(true));
    const admin = calls.filter((c) => c.url.startsWith('/__admin/') && !c.url.startsWith('/__admin/auth/config'));
    expect(admin.length).toBeGreaterThan(2);
    for (const c of admin) {
      expect(c.headers.get('x-fastmock-request'), c.url).toBe('1');
      expect(c.headers.get('authorization'), c.url).toBeNull();
      expect(c.credentials, c.url).toBe('same-origin');
    }
    expect(window.sessionStorage.getItem('fastmock.token')).toBeNull();
  });

  it('returns to the login page with a message when the session expires', async () => {
    let expired = false;
    server({ principal: user, session: true, spaces: SPACES.slice(1), expireSession: () => expired });
    renderApp(<AppRoutes />, '/stubs');
    const [row] = await screen.findAllByTestId('stub-row');
    if (!row) throw new Error('no row');
    expired = true;
    await userEvent.click(within(row).getByRole('switch'));
    expect(await screen.findByTestId('session-expired')).toBeInTheDocument();
  });

  it('signs out through the server and follows the provider logout URL', async () => {
    const assign = vi.spyOn(browser, 'assign').mockImplementation(() => undefined);
    const calls = server({ principal: user, session: true, spaces: SPACES.slice(1) });
    renderApp(<AppRoutes />, '/stubs');
    await screen.findAllByTestId('stub-row');
    await userEvent.click(screen.getByTestId('principal'));
    await userEvent.click(await screen.findByTestId('logout'));
    expect(await screen.findByTestId('token-input')).toBeInTheDocument();
    const logout = calls.find((c) => c.url === '/__admin/auth/logout');
    expect(logout?.method).toBe('POST');
    expect(logout?.headers.get('x-fastmock-request')).toBe('1');
    expect(logout?.credentials).toBe('same-origin');
    await waitFor(() => expect(assign).toHaveBeenCalledWith('https://idp.example.com/logout?x=1'));
  });

  it('keeps bearer tokens without cookies or the CSRF header in token mode', async () => {
    tokenStore.set('good');
    const calls = server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS } });
    renderApp(<AppRoutes />, '/stubs');
    await screen.findAllByTestId('stub-row');
    const admin = calls.filter((c) => c.url.startsWith('/__admin/') && !c.url.startsWith('/__admin/auth/'));
    for (const c of admin) {
      expect(c.headers.get('authorization')).toBe('Bearer good');
      expect(c.headers.get('x-fastmock-request')).toBeNull();
      expect(c.credentials).toBe('omit');
    }
    expect(screen.getByTestId('role-badge')).toHaveTextContent('admin');
    expect(screen.queryByTestId('space-select')).not.toBeInTheDocument();
  });
});

describe('spaces', () => {
  it('filters lists by the selected space, remembers it and shows a space column', async () => {
    tokenStore.set('good');
    const calls = server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, spaces: SPACES });
    renderApp(<AppRoutes />, '/stubs');
    expect(await screen.findAllByTestId('stub-row')).toHaveLength(2);
    expect(screen.getByRole('columnheader', { name: 'Space' })).toBeInTheDocument();
    const select = screen.getByRole('combobox', { name: 'Space' });
    expect(select).toHaveValue('All spaces');
    await userEvent.click(select);
    await userEvent.click(await screen.findByRole('option', { name: /payments/, hidden: true }));
    await waitFor(() => expect(screen.getAllByTestId('stub-row')).toHaveLength(1));
    expect(calls.some((c) => c.url.startsWith('/__admin/stubs?') && c.url.includes('space=payments'))).toBe(true);
    expect(window.localStorage.getItem('fastmock.space')).toBe('payments');
  });

  it('offers no "All spaces" to a principal bound to one space', async () => {
    tokenStore.set('good');
    const calls = server({
      principal: { name: 'ci', role: 'viewer', kind: 'apiToken', permissions: { global: ['info:read'], spaces: { payments: ['stubs:read'] } } },
      spaces: SPACES.slice(1),
    });
    renderApp(<AppRoutes />, '/stubs');
    await screen.findAllByTestId('stub-row');
    expect(screen.getByRole('combobox', { name: 'Space' })).toHaveValue('payments');
    expect(screen.getByTestId('kind-badge')).toHaveTextContent('managed token');
    await waitFor(() => expect(calls.some((c) => c.url.includes('/__admin/stubs?') && c.url.includes('space=payments'))).toBe(true));
  });
});

describe('permission-aware actions', () => {
  it('disables what the principal may not do and names the missing permission', async () => {
    tokenStore.set('good');
    server({
      principal: { name: 'alice', role: 'viewer', kind: 'user', permissions: { global: ['info:read'], spaces: { payments: ['stubs:read', 'stubs:write'], billing: ['stubs:read'] } } },
      spaces: SPACES,
      session: false,
    });
    renderApp(<AppRoutes />, '/stubs');
    const rows = await screen.findAllByTestId('stub-row');
    const [pay, bill] = rows;
    if (!pay || !bill) throw new Error('rows missing');
    expect(within(pay).getByRole('switch')).toBeEnabled();
    expect(within(bill).getByRole('switch')).toBeDisabled();
    const wrap = within(bill).getByTestId('missing-permission');
    expect(wrap).toHaveAttribute('data-permission', 'stubs:write');
    fireEvent.focus(wrap);
    expect(await screen.findByText('Requires the stubs:write permission in space billing')).toBeInTheDocument();
    expect(screen.getByTestId('new-stub')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: 'Import' })).not.toBeInTheDocument();
    expect(screen.queryByRole('link', { name: 'Admin tokens' })).not.toBeInTheDocument();
    expect(screen.queryByRole('link', { name: 'Audit log' })).not.toBeInTheDocument();
  });
});

describe('tokens page', () => {
  const OPTIONS = {
    maxDays: 30,
    defaultDays: 30,
    roles: [
      { name: 'viewer', builtin: true, permissions: ['stubs:read', 'info:read'], global: true, spaces: ['billing', 'payments'] },
      { name: 'admin', builtin: true, permissions: ['*'], global: false, spaces: [] },
      { name: 'stub-author', builtin: false, permissions: ['stubs:read', 'stubs:write'], global: true, spaces: ['payments'] },
    ],
  };
  const existing = {
    id: '0190e0d0-0000-7000-8000-000000000001',
    name: 'ci-billing',
    role: 'viewer',
    spaces: ['billing'],
    global: false,
    createdBy: 'ops',
    createdAt: '2026-11-01T10:00:00Z',
    expiresAt: '2027-01-01T10:00:00Z',
    lastUsedAt: null,
    revokedAt: null,
    status: 'active',
  };
  const VALUE = 'fm_' + 'ab'.repeat(32);

  function tokensServer() {
    let tokens = [existing];
    return mockFetch((url, init) => {
      if (url.pathname === '/openapi.json') return { body: { paths: { '/__admin/tokens': { get: {} } }, components: { schemas: {} } } };
      if (url.pathname === '/__admin/auth/config') return { body: { oidc: false, tokenLogin: true } };
      if (init.headers.get('authorization') !== 'Bearer good') return { status: 401, body: { error: 'unauthorized' } };
      if (url.pathname === '/__admin/info') return { body: info({ name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }) };
      if (url.pathname === '/__admin/spaces') return { body: { spaces: SPACES } };
      if (url.pathname === '/__admin/tokens/options') return { body: OPTIONS };
      if (url.pathname === '/__admin/tokens' && init.method === 'GET') return { body: { tokens } };
      if (url.pathname === '/__admin/tokens' && init.method === 'POST') {
        const b = JSON.parse(init.body ?? '{}') as { name: string; role: string; spaces?: string[] };
        const t = { ...existing, id: '0190e0d0-0000-7000-8000-000000000002', name: b.name, role: b.role, spaces: b.spaces ?? [], global: !b.spaces };
        tokens = [t, ...tokens];
        return { status: 201, body: { ...t, token: VALUE } };
      }
      if (url.pathname === `/__admin/tokens/${existing.id}` && init.method === 'DELETE') {
        tokens = tokens.filter((t) => t.id !== existing.id);
        return { body: { ...existing, status: 'revoked' } };
      }
      return undefined;
    });
  }

  it('creates a token, shows its value once, and revokes another with confirmation', async () => {
    tokenStore.set('good');
    const calls = tokensServer();
    renderApp(<AppRoutes />, '/tokens');
    const row = await screen.findByTestId('token-row');
    expect(row).toHaveTextContent('ci-billing');
    expect(row).toHaveTextContent('billing');

    await userEvent.click(screen.getByTestId('create-token'));
    await userEvent.type(await screen.findByTestId('token-name'), 'ci-payments');
    await userEvent.click(screen.getByRole('combobox', { name: 'Role' }));
    expect(screen.queryByRole('option', { name: /^admin/, hidden: true })).not.toBeInTheDocument();
    await userEvent.click(await screen.findByRole('option', { name: 'stub-author (custom)', hidden: true }));
    await userEvent.click(screen.getByTestId('token-spaces'));
    await userEvent.click(await screen.findByRole('option', { name: 'payments', hidden: true }));
    const days = screen.getByRole('textbox', { name: 'Lifetime (days)' });
    await userEvent.type(days, '31');
    fireEvent.blur(days);
    expect(days).toHaveValue('3');
    await userEvent.click(screen.getByTestId('token-create-submit'));

    const shown = await screen.findByTestId('token-value');
    expect(shown).toHaveTextContent(VALUE);
    expect(screen.getByTestId('show-once-warning')).toHaveTextContent(/only time/);
    const post = calls.find((c) => c.method === 'POST' && c.url === '/__admin/tokens');
    expect(JSON.parse(post?.body ?? '{}')).toEqual({ name: 'ci-payments', role: 'stub-author', spaces: ['payments'], expiresInDays: 3 });
    await userEvent.click(screen.getByTestId('token-done'));
    await waitFor(() => expect(screen.queryByTestId('token-value')).not.toBeInTheDocument());
    expect(document.body.textContent).not.toContain(VALUE);
    expect(JSON.stringify(window.localStorage)).not.toContain(VALUE);
    expect(JSON.stringify(window.sessionStorage)).not.toContain(VALUE);

    const old = (await screen.findAllByTestId('token-row')).find((r) => r.textContent.includes('ci-billing'));
    if (!old) throw new Error('row missing');
    await userEvent.click(within(old).getByTestId('revoke-token'));
    expect(calls.some((c) => c.method === 'DELETE')).toBe(false);
    await userEvent.click(await screen.findByTestId('confirm-button'));
    await waitFor(() => expect(calls.some((c) => c.method === 'DELETE' && c.url === `/__admin/tokens/${existing.id}`)).toBe(true));
  });

  it('is hidden from principals without tokens:read', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ci', role: 'editor', kind: 'token', permissions: { global: ['stubs:read', 'stubs:write', 'info:read'], spaces: {} } } });
    renderApp(<AppRoutes />, '/tokens');
    expect(await screen.findByText(/needs the tokens:read permission/)).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: 'Admin tokens' })).not.toBeInTheDocument();
  });
});
