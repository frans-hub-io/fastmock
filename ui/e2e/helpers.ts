import { expect, type Page } from '@playwright/test';

export const ADMIN_TOKEN = process.env.FASTMOCK_E2E_ADMIN_TOKEN ?? '';
export const VIEWER_TOKEN = process.env.FASTMOCK_E2E_VIEWER_TOKEN ?? '';
export const DATA_URL = process.env.FASTMOCK_E2E_DATA_URL ?? 'http://127.0.0.1:25080';
export const configured = ADMIN_TOKEN !== '' && VIEWER_TOKEN !== '';
export const LEAD_TOKEN = process.env.FASTMOCK_E2E_LEAD_TOKEN ?? '';
export const SPACE_VIEWER_TOKEN = process.env.FASTMOCK_E2E_SPACE_VIEWER_TOKEN ?? '';
/** The server runs with e2e/fixtures/access.yaml and the lead and pay-viewer static tokens. */
export const accessConfigured = configured && LEAD_TOKEN !== '' && SPACE_VIEWER_TOKEN !== '';
/** The server signs in through the fake IdP of e2e/fake-idp.ts (see sso.spec.ts). */
export const ssoConfigured = accessConfigured && process.env.FASTMOCK_E2E_SSO === '1';

export function watchConsole(page: Page): string[] {
  const errors: string[] = [];
  page.on('console', (m) => {
    if (m.type() === 'error') errors.push(m.text());
  });
  page.on('pageerror', (e) => errors.push(String(e)));
  return errors;
}

export function cspViolations(errors: string[]): string[] {
  return errors.filter((e) => /Content Security Policy|unsafe-eval|unsafe-inline/i.test(e));
}

export async function login(page: Page, token: string): Promise<void> {
  await page.goto('/ui/login');
  await page.getByTestId('token-input').fill(token);
  await page.getByTestId('login-submit').click();
  await expect(page.getByTestId('role-badge')).toBeVisible();
}

export async function adminApi(path: string, init: { method?: string; body?: unknown; token?: string } = {}): Promise<Response> {
  const base = process.env.FASTMOCK_E2E_URL ?? 'http://127.0.0.1:25090';
  return fetch(base + path, {
    method: init.method ?? 'GET',
    headers: { Authorization: `Bearer ${init.token ?? ADMIN_TOKEN}`, 'Content-Type': 'application/json' },
    body: init.body === undefined ? null : JSON.stringify(init.body),
  });
}
