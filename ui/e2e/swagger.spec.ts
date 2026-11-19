import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, configured, cspViolations, login, watchConsole } from './helpers';

test.skip(!configured, 'set FASTMOCK_E2E_ADMIN_TOKEN and FASTMOCK_E2E_VIEWER_TOKEN');

test('Swagger UI loads the spec and runs an authorized call', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'API docs' }).click();
  const host = page.getByTestId('swagger-host');
  await expect(host.getByText('FastMock Admin API')).toBeVisible({ timeout: 20_000 });
  const op = host.locator('#operations-system-info');
  await op.locator('.opblock-summary').click();
  await op.getByRole('button', { name: 'Execute' }).click();
  const response = op.locator('.live-responses-table .response .response-col_status').first();
  await expect(response).toHaveText('200');
  await expect(op.locator('.live-responses-table')).toContainText('"role": "admin"');
  expect(cspViolations(errors)).toEqual([]);
});
