import { expect, test } from '@playwright/test';
import { adminApi, cspViolations, ssoConfigured, watchConsole } from './helpers';

test.skip(
  !ssoConfigured,
  'run `node e2e/fake-idp.ts <port>` and start fastmock with FASTMOCK_OIDC_* pointing to it (see ui/README.md), then set FASTMOCK_E2E_SSO=1',
);

test('signs in with SSO, works in the user space with the session cookie and signs out', async ({ page }) => {
  const errors = watchConsole(page);
  const created = await adminApi('/__admin/stubs?space=payments', {
    method: 'POST',
    body: { name: 'sso e2e', request: { method: 'GET', urlPath: '/payments/sso-e2e' }, response: { status: 200 } },
  });
  const stub = (await created.json()) as { id: string };
  try {
    await page.goto('/ui/stubs');
    await expect(page).toHaveURL(/\/ui\/login$/);
    await expect(page.getByTestId('token-input')).toBeVisible();
    await page.getByTestId('sso-login').click();
    await page.getByRole('button', { name: 'Sign in as alice' }).click();
    await expect(page).toHaveURL(/\/ui\/stubs$/);
    await expect(page.getByTestId('principal-name')).toHaveText('alice');
    await expect(page.getByTestId('kind-badge')).toHaveText('user');
    await expect(page.getByTestId('role-badge')).toHaveText('editor in payments');
    expect(await page.evaluate(() => window.sessionStorage.getItem('fastmock.token'))).toBeNull();

    const csrf: string[] = [];
    page.on('request', (r) => {
      if (r.url().includes('/__admin/stubs/') && r.method() === 'PATCH') csrf.push(r.headers()['x-fastmock-request'] ?? '');
    });
    const row = page.getByTestId('stub-row').filter({ hasText: 'sso e2e' });
    await expect(row.getByRole('switch')).toBeEnabled();
    await row.getByRole('switch').click({ force: true });
    await expect(page.getByText('sso e2e disabled')).toBeVisible();
    expect(csrf).toEqual(['1']);

    await page.getByTestId('principal').click();
    await page.getByTestId('logout').click();
    await expect(page.getByRole('heading', { name: 'Signed out of the fake IdP' })).toBeVisible();
    await page.goto('/ui/');
    await expect(page.getByTestId('sso-login')).toBeVisible();
    expect(cspViolations(errors)).toEqual([]);
  } finally {
    await adminApi(`/__admin/stubs/${stub.id}`, { method: 'DELETE' });
  }
});

test('a user without bindings sees why the sign-in failed', async ({ page }) => {
  await page.goto('/ui/');
  await page.getByTestId('sso-login').click();
  await page.getByRole('button', { name: 'Sign in as mallory' }).click();
  await expect(page.getByTestId('login-error')).toContainText('no access to this FastMock server');
  await expect(page.getByTestId('token-input')).toBeVisible();
});
