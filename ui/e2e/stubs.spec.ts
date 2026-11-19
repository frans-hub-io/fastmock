import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, adminApi, configured, cspViolations, DATA_URL, login, watchConsole } from './helpers';

test.skip(!configured, 'set FASTMOCK_E2E_ADMIN_TOKEN and FASTMOCK_E2E_VIEWER_TOKEN');

const path = `/e2e/${Date.now().toString(36)}`;

test('create, edit, call, find in journal, verify and delete a stub', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'Stubs' }).first().click();
  await page.getByTestId('new-stub').click();

  await page.getByTestId('stub-name').fill('e2e greeting');
  await page.getByTestId('stub-url').fill(path);
  await page.getByTestId('stub-status').fill('201');
  await page.getByRole('button', { name: 'Add' }).nth(2).click();
  await page.getByLabel('Response headers name 2').fill('access-control-allow-origin');
  await page.getByLabel('Response headers value 2').fill('*');
  await expect(page.getByTestId('validation-ok')).toBeVisible();
  await page.getByTestId('save-stub').click();
  await expect(page).toHaveURL(/\/ui\/stubs\/[0-9a-f-]{36}$/);
  await expect(page.getByRole('heading', { name: 'e2e greeting' })).toBeVisible();

  await page.getByTestId('tab-json').click();
  const editor = page.getByTestId('stub-json-editor');
  await expect(editor).toContainText('"urlPath"');
  await page.getByRole('tab', { name: 'Form' }).click();
  await page.getByTestId('stub-status').fill('202');
  await page.getByTestId('save-stub').click();
  await expect(page.getByText('Saved e2e greeting').last()).toBeVisible();

  await page.getByTestId('tab-test').click();
  await page.getByTestId('dataplane-base').fill(DATA_URL);
  await page.getByTestId('send-test-request').click();
  await expect(page.getByTestId('test-result')).toContainText('202');
  await expect(page.getByTestId('curl-command')).toContainText(path);

  await page.getByRole('link', { name: 'Request journal' }).click();
  const row = page.getByTestId('journal-row').filter({ hasText: path }).first();
  await expect(row).toBeVisible();
  await row.click();
  await expect(page.getByRole('dialog', { name: 'Request detail' })).toContainText('matched');
  await page.keyboard.press('Escape');

  await page.getByRole('link', { name: 'Verify' }).click();
  await page.getByTestId('verify-url').fill(path);
  await page.getByTestId('run-verify').click();
  await expect(page.getByTestId('verify-count')).toHaveText('1');
  await page.locator('.mantine-SegmentedControl-label', { hasText: /^Verify$/ }).click();
  await page.getByTestId('run-verify').click();
  await expect(page.getByTestId('verify-result')).toContainText('Verified');

  await page.getByRole('link', { name: 'Stubs' }).first().click();
  await page.getByTestId('stub-search').fill(path);
  const stubRow = page.getByTestId('stub-row').filter({ hasText: 'e2e greeting' });
  await expect(stubRow).toHaveCount(1);
  await stubRow.getByRole('button', { name: /actions for/i }).click();
  await page.getByRole('menuitem', { name: 'Delete' }).click();
  await page.getByTestId('confirm-button').click();
  await expect(page.getByTestId('stub-row').filter({ hasText: 'e2e greeting' })).toHaveCount(0);

  const left = await (await adminApi(`/__admin/stubs?q=${encodeURIComponent(path)}`)).json() as { total: number };
  expect(left.total).toBe(0);
  expect(cspViolations(errors)).toEqual([]);
});
