import { expect, test } from '@playwright/test';
import { adminApi, configured, login, VIEWER_TOKEN } from './helpers';

test.skip(!configured, 'set FASTMOCK_E2E_ADMIN_TOKEN and FASTMOCK_E2E_VIEWER_TOKEN');

test('a viewer can read but not change anything', async ({ page }) => {
  const created = await adminApi('/__admin/stubs', {
    method: 'POST',
    body: { name: 'viewer sees me', request: { method: 'GET', urlPath: '/viewer-e2e' }, response: { status: 200 } },
  });
  const stub = (await created.json()) as { id: string };
  try {
    await login(page, VIEWER_TOKEN);
    await expect(page.getByTestId('role-badge')).toHaveText('viewer');
    await expect(page.getByRole('link', { name: 'Audit log' })).toHaveCount(0);
    await page.getByRole('link', { name: 'Stubs' }).first().click();
    const row = page.getByTestId('stub-row').filter({ hasText: 'viewer sees me' });
    await expect(row).toBeVisible();
    await expect(row.getByRole('switch')).toBeDisabled();
    await expect(page.getByTestId('new-stub')).toHaveCount(0);
    await page.getByRole('link', { name: 'viewer sees me' }).click();
    await expect(page.getByText('read-only')).toBeVisible();
    await expect(page.getByTestId('save-stub')).toHaveCount(0);
    await expect(page.getByTestId('stub-name')).toBeDisabled();
    await page.getByRole('link', { name: 'Request journal' }).click();
    await expect(page.getByRole('button', { name: 'Clear' })).toHaveCount(0);
    const forbidden = await page.evaluate(async (id) => {
      const t = window.sessionStorage.getItem('fastmock.token') ?? '';
      const r = await fetch(`/__admin/stubs/${id}`, { method: 'DELETE', headers: { Authorization: `Bearer ${t}` } });
      return r.status;
    }, stub.id);
    expect(forbidden).toBe(403);
  } finally {
    await adminApi(`/__admin/stubs/${stub.id}`, { method: 'DELETE' });
  }
});
