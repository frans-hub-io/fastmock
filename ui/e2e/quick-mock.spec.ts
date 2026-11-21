import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, adminApi, configured, cspViolations, DATA_URL, login, watchConsole } from './helpers';

test.skip(!configured, 'set FASTMOCK_E2E_ADMIN_TOKEN and FASTMOCK_E2E_VIEWER_TOKEN');

const run = Date.now().toString(36);

async function removeStubs(path: string): Promise<void> {
  const list = (await (await adminApi(`/__admin/stubs?q=${encodeURIComponent(path)}&limit=100`)).json()) as { stubs: { id: string }[] };
  for (const s of list.stubs) await adminApi(`/__admin/stubs/${s.id}`, { method: 'DELETE' });
}

test('build a protected mock from a template, create it and try it', async ({ page }) => {
  const errors = watchConsole(page);
  const path = `/e2e-quick/${run}/me`;
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'Quick mock' }).first().click();
  await expect(page.getByRole('heading', { name: 'Quick mock', level: 2 })).toBeVisible();

  await page.getByTestId('preset-protected').click();
  await page.getByTestId('quick-path').fill(path);
  await expect(page.getByTestId('quick-json')).toContainText(path);
  await expect(page.getByTestId('quick-valid')).toContainText('these 3 stubs');
  await page.locator('.mantine-SegmentedControl-label', { hasText: /^401 fallback$/ }).click();
  await expect(page.getByTestId('quick-json')).toContainText('"status": 401');

  await page.getByTestId('quick-create').click();
  await expect(page.getByText('Mock created: 3 stubs').last()).toBeVisible();
  await expect(page.getByTestId('tab-try')).toHaveAttribute('aria-selected', 'true');

  await page.getByTestId('quick-base').fill(DATA_URL);
  await page.getByTestId('quick-send').click();
  const result = page.getByTestId('quick-result');
  await expect(result).toContainText('200');
  await expect(result).toContainText('"id": 42');

  await page.locator('.mantine-SegmentedControl-label', { hasText: /^Python$/ }).click();
  await expect(page.getByTestId('quick-code')).toContainText('import requests');
  await expect(page.getByTestId('quick-code')).toContainText('"Authorization": "Bearer demo-token"');
  await page.locator('.mantine-SegmentedControl-label', { hasText: /^Java$/ }).click();
  await expect(page.getByTestId('quick-code')).toContainText('HttpClient.newHttpClient()');
  await expect(page.getByRole('button', { name: 'Copy Java code' })).toBeVisible();

  const anonymous = await fetch(DATA_URL + path);
  expect(anonymous.status).toBe(401);
  const preflight = await fetch(DATA_URL + path, { method: 'OPTIONS', headers: { Origin: 'http://example.com', 'Access-Control-Request-Method': 'GET' } });
  expect(preflight.status).toBe(204);
  expect(preflight.headers.get('access-control-allow-headers')).toBe('authorization');

  await page.getByRole('tab', { name: 'Stub JSON' }).click();
  await expect(page.getByTestId('quick-created')).toContainText('Mock is live');
  await page.getByTestId('behaviour-delay').getByText('Delay', { exact: true }).click();
  await page.getByTestId('quick-create').click();
  await expect(page.getByText('Mock updated').last()).toBeVisible();
  const stored = (await (await adminApi(`/__admin/stubs?q=${encodeURIComponent(path)}&limit=100`)).json()) as { total: number; stubs: { response?: { delay?: unknown }; tags?: string[] }[] };
  expect(stored.total).toBe(3);
  expect(stored.stubs.some((s) => s.response?.delay)).toBe(true);
  expect(stored.stubs.every((s) => s.tags?.includes('quick-mock'))).toBe(true);

  await removeStubs(path);
  expect(cspViolations(errors)).toEqual([]);
});

test('a paginated list reads page and size from the query', async ({ page }) => {
  const errors = watchConsole(page);
  const path = `/e2e-quick/${run}/items`;
  await login(page, ADMIN_TOKEN);
  await page.goto('/ui/quick-mock');
  await page.getByTestId('preset-paginated').click();
  await page.getByTestId('quick-path').fill(path);
  await page.getByTestId('quick-status').fill('201');
  await expect(page.getByRole('group', { name: 'Common statuses' }).getByRole('button', { name: '201' })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByTestId('quick-valid')).toBeVisible();
  await page.getByTestId('quick-create').click();
  await expect(page.getByTestId('quick-result')).toHaveCount(0);

  await page.getByTestId('quick-base').fill(DATA_URL);
  await page.getByTestId('quick-target').fill(`${path}?page=3&size=5`);
  await page.getByTestId('quick-send').click();
  const result = page.getByTestId('quick-result');
  await expect(result).toContainText('201');
  await expect(result).toContainText('"page": 3');
  await expect(result).toContainText('"nextPage": 4');
  await expect(result).toContainText('"size": 5');

  await removeStubs(path);
  expect(cspViolations(errors)).toEqual([]);
});
