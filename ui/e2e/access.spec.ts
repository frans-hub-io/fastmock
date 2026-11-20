import { expect, test, type Page } from '@playwright/test';
import { accessConfigured, ADMIN_TOKEN, adminApi, cspViolations, LEAD_TOKEN, login, SPACE_VIEWER_TOKEN, watchConsole } from './helpers';

test.skip(!accessConfigured, 'start fastmock with e2e/fixtures/access.yaml and set FASTMOCK_E2E_LEAD_TOKEN and FASTMOCK_E2E_SPACE_VIEWER_TOKEN');

const run = `e2e${Date.now().toString(36)}`;
const created: string[] = [];

async function createStub(space: string | null, name: string, urlPath: string): Promise<void> {
  const r = await adminApi(`/__admin/stubs${space ? `?space=${space}` : ''}`, {
    method: 'POST',
    body: { name, request: { method: 'GET', urlPath }, response: { status: 200, body: name } },
  });
  expect(r.status, await r.clone().text()).toBe(201);
  created.push(((await r.json()) as { id: string }).id);
}

async function pickSpace(page: Page, name: string): Promise<void> {
  await page.getByTestId('space-select').click();
  await page.getByRole('option', { name: new RegExp(`^${name}`) }).click();
}

async function stubRows(page: Page) {
  await page.getByRole('link', { name: 'Stubs' }).first().click();
  await page.getByTestId('stub-search').fill(run);
  return page.getByTestId('stub-row');
}

test.beforeAll(async () => {
  await createStub(null, `${run} global`, `/${run}/global`);
  await createStub('payments', `${run} payments`, `/payments/${run}`);
  await createStub('billing', `${run} billing`, `/${run}/billing`);
});

test.afterAll(async () => {
  for (const id of created) await adminApi(`/__admin/stubs/${id}`, { method: 'DELETE' });
});

test('the space selector filters lists, shows the space column and is remembered', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, ADMIN_TOKEN);
  await expect(page.getByTestId('role-badge')).toHaveText('admin');
  await expect(page.getByTestId('kind-badge')).toHaveText('token');
  await expect(page.getByTestId('space-select')).toHaveValue('All spaces');
  const rows = await stubRows(page);
  await expect(rows).toHaveCount(3);
  await expect(page.getByTestId('stubs-table').locator('th', { hasText: 'Space' })).toBeVisible();

  await pickSpace(page, 'payments');
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText(`${run} payments`);
  await expect(rows.first().getByTestId('space-badge')).toHaveText('payments');

  await page.reload();
  await expect(page.getByTestId('space-select')).toHaveValue('payments');
  await page.getByTestId('stub-search').fill(run);
  await expect(page.getByTestId('stub-row')).toHaveCount(1);

  await page.getByTestId('new-stub').click();
  await expect(page.getByTestId('space-field')).toHaveValue('payments');
  await pickSpace(page, 'All spaces');
  expect(cspViolations(errors)).toEqual([]);
});

test('a viewer bound to one space sees only that space and no write actions', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, SPACE_VIEWER_TOKEN);
  await expect(page.getByTestId('role-badge')).toHaveText('payments');
  await expect(page.getByTestId('space-select')).toHaveValue('payments');
  await page.getByTestId('space-select').click();
  await expect(page.getByRole('option')).toHaveCount(1);
  await expect(page.getByRole('option')).toContainText('payments');
  await page.keyboard.press('Escape');

  const rows = await stubRows(page);
  await expect(rows).toHaveCount(1);
  await expect(rows.first()).toContainText(`${run} payments`);
  await expect(page.getByTestId('new-stub')).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Import' })).toHaveCount(0);
  await expect(rows.first().getByRole('switch')).toBeDisabled();
  await rows.first().getByTestId('missing-permission').hover();
  await expect(page.getByRole('tooltip')).toHaveText('Requires the stubs:write permission in space payments');
  await expect(page.getByRole('link', { name: 'Admin tokens' })).toHaveCount(0);
  await expect(page.getByRole('link', { name: 'Audit log' })).toHaveCount(0);

  const status = await page.evaluate(async (id) => {
    const t = window.sessionStorage.getItem('fastmock.token') ?? '';
    const r = await fetch(`/__admin/stubs/${id}`, { method: 'DELETE', headers: { Authorization: `Bearer ${t}` } });
    return r.status;
  }, created[1]);
  expect(status).toBe(403);
  expect(cspViolations(errors)).toEqual([]);
});

test('tokens page: create, show the value once, use it and revoke it', async ({ page }) => {
  const errors = watchConsole(page);
  const name = `${run}-ci`;
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'Admin tokens' }).click();
  await expect(page.getByRole('heading', { name: 'Admin tokens' })).toBeVisible();
  await page.getByTestId('create-token').click();
  await page.getByTestId('token-name').fill(name);
  await page.getByTestId('token-role').click();
  await page.getByRole('option', { name: 'viewer' }).click();
  await expect(page.getByTestId('token-role-permissions')).toContainText('stubs:read');
  await page.getByTestId('token-spaces').click();
  await page.getByRole('option', { name: 'billing' }).click();
  await page.keyboard.press('Escape');
  await page.getByTestId('token-days').fill('7');
  await page.getByTestId('token-create-submit').click();

  const shown = page.getByTestId('token-value');
  await expect(shown).toBeVisible();
  const value = (await shown.textContent())?.trim() ?? '';
  expect(value).toMatch(/^fm_[0-9a-f]{64}$/);
  await expect(page.getByTestId('show-once-warning')).toContainText('only time');

  const me = await adminApi('/__admin/info', { token: value });
  expect(me.status).toBe(200);
  const principal = ((await me.json()) as { principal: { kind: string; permissions: { spaces: Record<string, string[]> } } }).principal;
  expect(principal.kind).toBe('apiToken');
  expect(Object.keys(principal.permissions.spaces)).toEqual(['billing']);

  await page.getByTestId('token-done').click();
  await expect(shown).toHaveCount(0);
  expect(await page.content()).not.toContain(value);
  const row = page.getByTestId('token-row').filter({ hasText: name });
  await expect(row).toContainText('viewer');
  await expect(row).toContainText('billing');
  await expect(row).toContainText('active');

  await row.getByTestId('revoke-token').click();
  await page.getByTestId('confirm-button').click();
  await expect(row).toHaveCount(0);
  expect((await adminApi('/__admin/info', { token: value })).status).toBe(401);
  await page.locator('label', { hasText: /^Revoked$/ }).click();
  await expect(page.getByTestId('token-row').filter({ hasText: name })).toContainText('revoked');
  expect(cspViolations(errors)).toEqual([]);
});

test('a team lead can grant only roles it holds, only in its space', async ({ page }) => {
  await login(page, LEAD_TOKEN);
  await page.getByRole('link', { name: 'Admin tokens' }).click();
  await page.getByTestId('create-token').click();
  await page.getByTestId('token-role').click();
  await expect(page.getByRole('option')).toHaveText(['viewer', 'team-lead (custom)']);
  await page.getByRole('option', { name: 'team-lead (custom)' }).click();
  await expect(page.getByTestId('token-scope')).toHaveCount(0);
  await page.getByTestId('token-spaces').click();
  await expect(page.getByRole('option')).toHaveText(['payments']);
});

test('access page: a space viewer sees only its own access', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, SPACE_VIEWER_TOKEN);
  await page.getByRole('link', { name: 'Access' }).click();
  await expect(page.getByRole('heading', { name: 'Access', exact: true })).toBeVisible();
  await expect(page.getByTestId('your-access')).toContainText('pay-viewer');
  await expect(page.getByTestId('your-space-row')).toHaveCount(1);
  await expect(page.getByTestId('your-space-row')).toContainText('payments');
  await expect(page.getByTestId('your-space-row')).toContainText('stubs:read');
  await expect(page.getByTestId('access-config-hidden')).toBeVisible();
  await expect(page.getByTestId('access-config')).toHaveCount(0);
  expect(cspViolations(errors)).toEqual([]);
});

test('access page: access:read shows the spaces, roles and bindings of the access file', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'Access' }).click();
  await expect(page.getByTestId('access-source')).toContainText('access file');
  await expect(page.getByTestId('access-space-row')).toHaveText([/^billing/, /^payments\/payments\//]);
  await expect(page.getByTestId('access-role-row').filter({ hasText: 'team-lead' })).toContainText('custom');
  await expect(page.getByTestId('access-role-row').first()).toContainText(/viewer\s*built-in/);
  await expect(page.getByTestId('access-binding-row').first()).toHaveText('token:leadteam-leadpayments');
  expect(cspViolations(errors)).toEqual([]);
});

test('tokens page: a token for all spaces', async ({ page }) => {
  const errors = watchConsole(page);
  const name = `${run}-all`;
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'Admin tokens' }).click();
  await page.getByTestId('create-token').click();
  await page.getByTestId('token-name').fill(name);
  await page.getByTestId('token-role').click();
  await page.getByRole('option', { name: 'viewer' }).click();
  await page.getByTestId('token-scope').locator('label', { hasText: /^All spaces$/ }).click();
  await expect(page.getByTestId('token-all-spaces-note')).toBeVisible();
  await page.getByTestId('token-days').fill('1');
  await page.getByTestId('token-create-submit').click();

  const shown = page.getByTestId('token-value');
  await expect(shown).toBeVisible();
  const value = (await shown.textContent())?.trim() ?? '';
  const me = await adminApi('/__admin/info', { token: value });
  expect(me.status).toBe(200);
  const perms = ((await me.json()) as { principal: { permissions: { spaces: Record<string, string[]>; allSpaces: string[] } } }).principal.permissions;
  expect(Object.keys(perms.spaces).sort()).toEqual(['billing', 'payments']);
  expect(perms.allSpaces).toContain('stubs:read');

  await page.getByTestId('token-done').click();
  const row = page.getByTestId('token-row').filter({ hasText: name });
  await expect(row.getByTestId('scope-all-spaces')).toHaveText('All spaces');
  await row.getByTestId('revoke-token').click();
  await page.getByTestId('confirm-button').click();
  await expect(row).toHaveCount(0);
  expect(cspViolations(errors)).toEqual([]);
});
