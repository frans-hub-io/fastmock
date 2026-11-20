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
  role: string;
  kind?: string;
  permissions?: { global: string[]; spaces: Record<string, string[] | null>; allSpaces?: string[] };
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
  /** `features` of `GET /__admin/version`. */
  features?: string[];
  /** Body of `GET /__admin/access`; 403 when absent. */
  access?: unknown;
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
    if (url.pathname === '/__admin/version') return { body: { version: '0.1.0', features: s.features ?? ['files', 'tokens'] } };
    if (url.pathname === '/__admin/spaces') return { body: { spaces: s.spaces ?? [] } };
    if (url.pathname === '/__admin/access') return s.access ? { body: s.access } : { status: 403, body: { error: 'forbidden', details: [], required: 'access:read' } };
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

  it('stays out of session mode when a proxy authenticates a token principal, and signs out locally', async () => {
    const calls = server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, session: true, oidc: true });
    renderApp(<AppRoutes />, '/stubs');
    await screen.findAllByTestId('stub-row');
    await userEvent.click(screen.getByTestId('principal'));
    await userEvent.click(await screen.findByTestId('logout'));
    expect(await screen.findByTestId('token-input')).toBeInTheDocument();
    expect(calls.some((c) => c.url.startsWith('/__admin/auth/logout'))).toBe(false);
    expect(screen.queryByTestId('session-expired')).not.toBeInTheDocument();
  });

  it('uses session mode for a principal without a kind only when SSO is configured', async () => {
    const legacy: Principal = { name: 'ops', role: 'admin', permissions: ADMIN_PERMS };
    for (const oidc of [true, false]) {
      tokenStore.setSession(false);
      const calls = server({ principal: legacy, session: true, oidc });
      const view = renderApp(<AppRoutes />, '/stubs');
      await screen.findAllByTestId('stub-row');
      await userEvent.click(screen.getByTestId('principal'));
      await userEvent.click(await screen.findByTestId('logout'));
      await screen.findByTestId('token-input');
      expect(calls.some((c) => c.url === '/__admin/auth/logout'), `oidc=${oidc}`).toBe(oidc);
      view.unmount();
      vi.unstubAllGlobals();
    }
  });
});

describe('unknown roles', () => {
  it('renders the shell for a role the UI does not know and grants it nothing', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'auditor-1', role: 'auditor', kind: 'token' }, spaces: SPACES });
    renderApp(<AppRoutes />, '/');
    expect(await screen.findByTestId('principal-name')).toHaveTextContent('auditor-1');
    expect(screen.getByTestId('role-badge')).toHaveTextContent('auditor');
    expect(screen.queryByRole('link', { name: 'Stubs' })).not.toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Dashboard' })).toBeInTheDocument();
  });

  it('tolerates a space without a permission list', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ci', role: 'custom', kind: 'apiToken', permissions: { global: ['info:read'], spaces: { billing: null, payments: ['stubs:read'] } } }, spaces: SPACES });
    renderApp(<AppRoutes />, '/stubs');
    expect(await screen.findByTestId('principal-name')).toHaveTextContent('ci');
    expect(await screen.findByRole('link', { name: 'Stubs' })).toBeInTheDocument();
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
      if (url.pathname === '/__admin/version') return { body: { version: '0.1.0', features: ['tokens'] } };
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

  it('is hidden from admins when the server does not advertise the tokens API', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, features: ['files'] });
    renderApp(<AppRoutes />, '/tokens');
    expect(await screen.findByText('Not available on this server')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Audit log' })).toBeInTheDocument();
    expect(screen.queryByRole('link', { name: 'Admin tokens' })).not.toBeInTheDocument();
  });

  it('is shown to admins when the server advertises the tokens API', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, features: ['files', 'tokens'] });
    renderApp(<AppRoutes />, '/');
    expect(await screen.findByRole('link', { name: 'Admin tokens' })).toBeInTheDocument();
  });

  it('counts an explicit tokens permission as the API being offered', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ci', role: 'viewer', kind: 'apiToken', permissions: { global: ['info:read', 'tokens:read'], spaces: {} } }, features: [] });
    renderApp(<AppRoutes />, '/');
    expect(await screen.findByRole('link', { name: 'Admin tokens' })).toBeInTheDocument();
  });

  it('is hidden from principals without tokens:read', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ci', role: 'editor', kind: 'token', permissions: { global: ['stubs:read', 'stubs:write', 'info:read'], spaces: {} } } });
    renderApp(<AppRoutes />, '/tokens');
    expect(await screen.findByTestId('no-access')).toHaveTextContent(/requires the tokens:read permission/);
    expect(screen.queryByRole('link', { name: 'Admin tokens' })).not.toBeInTheDocument();
  });
});

describe('screens and imports outside the principal permissions', () => {
  const spaceEditor: Principal = {
    name: 'carol',
    role: 'editor',
    kind: 'user',
    permissions: { global: ['info:read'], spaces: { payments: ['stubs:read', 'stubs:write', 'imports:run'] } },
  };

  it('shows the no-access state on screens opened by URL without their permission', async () => {
    tokenStore.set('good');
    server({ principal: spaceEditor, spaces: SPACES.slice(1) });
    for (const [path, perm] of [
      ['/grpc', 'grpc:read'],
      ['/recordings', 'recordings:read'],
      ['/services', 'stubs:read'],
      ['/audit', 'audit:read'],
    ] as const) {
      const view = renderApp(<AppRoutes />, path);
      const state = await screen.findByTestId('no-access');
      expect(state, path).toHaveTextContent(`requires the ${perm} permission for the whole server`);
      expect(screen.queryByText('forbidden'), path).not.toBeInTheDocument();
      view.unmount();
    }
  });

  it('turns a 403 answer into the no-access state', async () => {
    tokenStore.set('good');
    mockFetch((url) => {
      if (url.pathname === '/__admin/info') return { body: info({ name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }) };
      if (url.pathname === '/__admin/version') return { body: { version: '0.1.0', features: [] } };
      if (url.pathname === '/__admin/spaces') return { body: { spaces: [] } };
      if (url.pathname === '/__admin/stubs') return { status: 403, body: { error: 'forbidden', details: ['requires stubs:read'], required: 'stubs:read' } };
      return undefined;
    });
    renderApp(<AppRoutes />, '/stubs');
    expect(await screen.findByTestId('no-access')).toHaveTextContent('requires the stubs:read permission');
  });

  it('offers global-only import formats only to principals that can import globally', async () => {
    tokenStore.set('good');
    server({ principal: spaceEditor, spaces: SPACES.slice(1) });
    const view = renderApp(<AppRoutes />, '/stubs');
    await userEvent.click(await screen.findByRole('button', { name: 'Import' }));
    const dropzone = await screen.findByTestId('import-dropzone');
    expect(dropzone).not.toHaveTextContent(/AsyncAPI|WSDL/);
    const input = dropzone.querySelector('input[type="file"]');
    if (!(input instanceof HTMLInputElement)) throw new Error('no file input');
    await userEvent.upload(input, new File(['{"asyncapi":"2.6.0","channels":{}}'], 'events.json', { type: 'application/json' }));
    expect(await screen.findByTestId('import-global-only')).toHaveTextContent(/imports:run permission for\s+the whole server/);
    expect(screen.getByTestId('import-submit')).toBeDisabled();
    view.unmount();

    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, spaces: SPACES });
    renderApp(<AppRoutes />, '/stubs');
    await userEvent.click(await screen.findByRole('button', { name: 'Import' }));
    expect(await screen.findByTestId('import-dropzone')).toHaveTextContent(/AsyncAPI, WSDL/);
  });
});

describe('access page', () => {
  const BUILTIN = [
    { name: 'viewer', builtin: true, permissions: ['stubs:read', 'journal:read'] },
    { name: 'editor', builtin: true, permissions: ['stubs:read', 'stubs:write'] },
    { name: 'admin', builtin: true, permissions: ['access:read', 'stubs:purge'] },
  ];
  const CONFIGURED = {
    configured: true,
    oidc: true,
    spaces: [
      { name: 'billing', pathPrefixes: ['/billing/'], hosts: ['billing.example.com'] },
      { name: 'payments', pathPrefixes: ['/payments/'], hosts: [] },
    ],
    roles: [...BUILTIN, { name: 'team-lead', builtin: false, permissions: ['stubs:read', 'tokens:write'] }],
    bindings: [
      { subject: 'token:ci-payments', role: 'team-lead', global: false, spaces: ['payments'] },
      { subject: 'group:qa', role: 'editor', global: false, spaces: ['*'] },
      { subject: 'group:platform', role: 'admin', global: true, spaces: [] },
    ],
  };

  it('shows your access and the read-only configuration to principals with access:read', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, spaces: SPACES, access: CONFIGURED });
    renderApp(<AppRoutes />, '/access');
    expect(await screen.findByRole('heading', { name: 'Access' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Access' })).toBeInTheDocument();
    expect(screen.getByTestId('your-global')).toHaveTextContent('*');
    expect(await screen.findByTestId('access-source')).toHaveTextContent(/access file.*restart/);
    expect(screen.getByTestId('oidc-badge')).toHaveTextContent('OIDC enabled');

    const spaces = screen.getAllByTestId('access-space-row');
    expect(spaces.map((r) => r.textContent)).toEqual(['billing/billing/billing.example.com', 'payments/payments/—']);

    const roles = screen.getAllByTestId('access-role-row');
    expect(roles).toHaveLength(4);
    expect(roles[0]).toHaveTextContent(/viewer\s*built-in/);
    expect(roles[3]).toHaveTextContent(/team-lead\s*custom/);
    expect(roles[3]).toHaveTextContent('tokens:write');

    const bindings = screen.getAllByTestId('access-binding-row');
    expect(bindings.map((r) => r.textContent)).toEqual(['token:ci-paymentsteam-leadpayments', 'group:qaeditorAll spaces', 'group:platformadminGlobal']);
    expect(within(bindings[1] as HTMLElement).getByTestId('scope-all-spaces')).toBeInTheDocument();
    expect(screen.queryByRole('button', { name: /edit|delete|save/i })).not.toBeInTheDocument();
  });

  it('shows only your access to principals without access:read and does not call the endpoint', async () => {
    tokenStore.set('good');
    const calls = server({
      principal: { name: 'alice', role: 'viewer', kind: 'user', groups: ['payments-dev'], permissions: { global: ['info:read', 'journal:read'], spaces: { payments: ['stubs:read', 'stubs:write'] } } },
      spaces: SPACES.slice(1),
      access: CONFIGURED,
    });
    renderApp(<AppRoutes />, '/access');
    expect(await screen.findByTestId('your-access')).toHaveTextContent(/alice.*user.*payments-dev/);
    expect(screen.getByTestId('your-global')).toHaveTextContent('info:readjournal:read');
    const [row] = screen.getAllByTestId('your-space-row');
    expect(row).toHaveTextContent('payments');
    expect(row).toHaveTextContent('journal:readstubs:readstubs:write');
    expect(row).not.toHaveTextContent('info:read');
    expect(screen.getByTestId('access-config-hidden')).toHaveTextContent('access:read');
    expect(screen.queryByTestId('access-config')).not.toBeInTheDocument();
    expect(screen.queryByTestId('your-all-spaces')).not.toBeInTheDocument();
    expect(calls.some((c) => c.url.startsWith('/__admin/access'))).toBe(false);
  });

  it('explains static tokens and built-in roles when no access file is loaded', async () => {
    tokenStore.set('good');
    server({ principal: { name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }, access: { configured: false, oidc: false, spaces: [], roles: BUILTIN, bindings: [] } });
    renderApp(<AppRoutes />, '/access');
    const note = await screen.findByTestId('access-not-configured');
    expect(note).toHaveTextContent(/static admin tokens and the built-in roles/);
    expect(note).toHaveTextContent('operations/access-control/');
    expect(screen.getByTestId('access-file-badge')).toHaveTextContent('No access file');
    expect(screen.getAllByTestId('access-role-row')).toHaveLength(3);
    expect(screen.queryByTestId('access-spaces')).not.toBeInTheDocument();
    expect(screen.queryByTestId('access-bindings')).not.toBeInTheDocument();
    expect(screen.queryByTestId('access-source')).not.toBeInTheDocument();
  });

  it('shows permissions granted in all spaces', async () => {
    tokenStore.set('good');
    server({
      principal: {
        name: 'qa',
        role: 'viewer',
        kind: 'apiToken',
        permissions: { global: ['info:read'], spaces: { billing: ['stubs:read', 'journal:read'], payments: ['stubs:read', 'journal:read'] }, allSpaces: ['stubs:read', 'journal:read'] },
      },
      spaces: SPACES,
    });
    renderApp(<AppRoutes />, '/access');
    const all = await screen.findByTestId('your-all-spaces');
    expect(all).toHaveTextContent(/All spaces.*including spaces added later.*stubs:readjournal:read/);
    expect(screen.getAllByTestId('your-space-row')).toHaveLength(2);
    expect(screen.getByTestId('role-badge')).toHaveTextContent('all spaces');
    expect(screen.getByTestId('access-config-hidden')).toBeInTheDocument();
  });
});

describe('tokens in all spaces', () => {
  const OPTIONS = {
    maxDays: 30,
    defaultDays: 30,
    roles: [
      { name: 'viewer', builtin: true, permissions: ['stubs:read'], global: true, allSpaces: true, spaces: ['billing', 'payments'] },
      { name: 'stub-author', builtin: false, permissions: ['stubs:read', 'stubs:write'], global: false, allSpaces: false, spaces: ['payments'] },
      { name: 'qa', builtin: false, permissions: ['journal:read'], global: false, allSpaces: true, spaces: [] },
    ],
  };
  const STAR = {
    id: '0190e0d0-0000-7000-8000-000000000003',
    name: 'ci-everywhere',
    role: 'viewer',
    spaces: ['*'],
    global: false,
    createdBy: 'ops',
    createdAt: '2026-11-01T10:00:00Z',
    expiresAt: '2027-01-01T10:00:00Z',
    lastUsedAt: null,
    revokedAt: null,
    status: 'active',
  };

  function starServer() {
    let tokens = [STAR];
    return mockFetch((url, init) => {
      if (url.pathname === '/openapi.json') return { body: { paths: { '/__admin/tokens': { get: {} } }, components: { schemas: {} } } };
      if (url.pathname === '/__admin/auth/config') return { body: { oidc: false, tokenLogin: true } };
      if (init.headers.get('authorization') !== 'Bearer good') return { status: 401, body: { error: 'unauthorized' } };
      if (url.pathname === '/__admin/info') return { body: info({ name: 'ops', role: 'admin', kind: 'token', permissions: ADMIN_PERMS }) };
      if (url.pathname === '/__admin/version') return { body: { version: '0.1.0', features: ['tokens'] } };
      if (url.pathname === '/__admin/spaces') return { body: { spaces: SPACES } };
      if (url.pathname === '/__admin/tokens/options') return { body: OPTIONS };
      if (url.pathname === '/__admin/tokens' && init.method === 'GET') return { body: { tokens } };
      if (url.pathname === '/__admin/tokens' && init.method === 'POST') {
        const b = JSON.parse(init.body ?? '{}') as { name: string; role: string; spaces?: string[] };
        const t = { ...STAR, id: '0190e0d0-0000-7000-8000-000000000004', name: b.name, role: b.role, spaces: b.spaces ?? [], global: !b.spaces };
        tokens = [t, ...tokens];
        return { status: 201, body: { ...t, token: 'fm_' + 'cd'.repeat(32) } };
      }
      return undefined;
    });
  }

  async function pickRole(name: string) {
    await userEvent.click(screen.getByRole('combobox', { name: 'Role' }));
    await userEvent.click(await screen.findByRole('option', { name, hidden: true }));
  }

  it('shows All spaces in the list and creates a token for all spaces', async () => {
    tokenStore.set('good');
    const calls = starServer();
    renderApp(<AppRoutes />, '/tokens');
    const row = await screen.findByTestId('token-row');
    expect(within(row).getByTestId('scope-all-spaces')).toHaveTextContent('All spaces');
    expect(within(row).getByTestId('revoke-token')).toBeEnabled();

    await userEvent.click(screen.getByTestId('create-token'));
    await userEvent.type(await screen.findByTestId('token-name'), 'ci-all');
    await pickRole('viewer');
    const scope = screen.getByTestId('token-scope');
    expect(within(scope).getAllByRole('radio').map((r) => r.getAttribute('value'))).toEqual(['spaces', 'all', 'global']);
    await userEvent.click(within(scope).getByLabelText('All spaces'));
    expect(screen.getByTestId('token-all-spaces-note')).toHaveTextContent(/including spaces added later/);
    expect(screen.queryByTestId('token-spaces')).not.toBeInTheDocument();
    await userEvent.click(screen.getByTestId('token-create-submit'));
    await screen.findByTestId('token-value');
    const post = calls.find((c) => c.method === 'POST' && c.url === '/__admin/tokens');
    expect(JSON.parse(post?.body ?? '{}')).toEqual({ name: 'ci-all', role: 'viewer', spaces: ['*'], expiresInDays: 30 });
  });

  it('offers All spaces only for roles whose allSpaces is true', async () => {
    tokenStore.set('good');
    starServer();
    renderApp(<AppRoutes />, '/tokens');
    await userEvent.click(await screen.findByTestId('create-token'));
    await screen.findByTestId('token-name');
    await pickRole('stub-author (custom)');
    expect(screen.queryByTestId('token-scope')).not.toBeInTheDocument();
    expect(screen.getByTestId('token-spaces')).toBeInTheDocument();
    await pickRole('qa (custom)');
    expect(screen.queryByTestId('token-scope')).not.toBeInTheDocument();
    expect(screen.queryByTestId('token-spaces')).not.toBeInTheDocument();
    expect(screen.getByTestId('token-all-spaces-note')).toBeInTheDocument();
    expect(screen.getByTestId('token-create-submit')).toBeDisabled();
    await userEvent.type(screen.getByTestId('token-name'), 'ci-qa');
    expect(screen.getByTestId('token-create-submit')).toBeEnabled();
  });
});
