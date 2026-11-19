import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, configured, cspViolations, login, watchConsole } from './helpers';

test.skip(!configured, 'set FASTMOCK_E2E_ADMIN_TOKEN and FASTMOCK_E2E_VIEWER_TOKEN');

test('rejects a wrong token, signs in, shows the principal and signs out', async ({ page }) => {
  const errors = watchConsole(page);
  await page.goto('/ui/');
  await expect(page).toHaveURL(/\/ui\/login$/);
  await page.getByTestId('token-input').fill('fm_wrong');
  await page.getByTestId('login-submit').click();
  await expect(page.getByText('The server rejected this token')).toBeVisible();

  await login(page, ADMIN_TOKEN);
  await expect(page.getByTestId('role-badge')).toHaveText('admin');
  await expect(page.getByRole('heading', { name: 'Dashboard' })).toBeVisible();
  expect(await page.evaluate(() => window.localStorage.getItem('fastmock.token'))).toBeNull();
  expect(await page.evaluate(() => window.sessionStorage.getItem('fastmock.token'))).toBe(ADMIN_TOKEN);

  await page.getByTestId('principal').click();
  await page.getByTestId('logout').click();
  await expect(page.getByTestId('login-submit')).toBeVisible();
  expect(await page.evaluate(() => window.sessionStorage.getItem('fastmock.token'))).toBeNull();
  expect(cspViolations(errors)).toEqual([]);
});
