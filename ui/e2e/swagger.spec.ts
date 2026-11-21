import { expect, test, type Locator, type Page } from '@playwright/test';
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

/**
 * The colours a person sees for the text and the background of a form field: its computed colour and the first
 * opaque background behind it, both passed through the CSS filters (invert, hue-rotate) of its ancestors, and the
 * WCAG contrast ratio between them.
 */
async function seenContrast(field: Locator): Promise<{ fg: number[]; bg: number[]; ratio: number }> {
  return field.evaluate((el) => {
    const parse = (c: string): number[] => {
      const m = /rgba?\(([^)]+)\)/.exec(c);
      if (!m?.[1]) throw new Error(`unexpected colour ${c}`);
      const p = m[1].split(/[ ,/]+/).filter(Boolean).map(Number);
      return [p[0] ?? 0, p[1] ?? 0, p[2] ?? 0, p[3] ?? 1];
    };
    const applyFilter = (rgb: number[], filter: string): number[] => {
      let [r, g, b] = rgb.map((v) => v / 255) as [number, number, number];
      for (const f of filter.matchAll(/([a-z-]+)\(([^)]*)\)/g)) {
        const name = f[1];
        const arg = f[2] ?? '';
        if (name === 'invert') {
          const a = arg.endsWith('%') ? parseFloat(arg) / 100 : parseFloat(arg || '1');
          [r, g, b] = [r, g, b].map((v) => a * (1 - v) + (1 - a) * v) as [number, number, number];
        } else if (name === 'hue-rotate') {
          const deg = parseFloat(arg) * (arg.endsWith('rad') ? 180 / Math.PI : 1);
          const t = (deg * Math.PI) / 180;
          const c = Math.cos(t);
          const s = Math.sin(t);
          const m: [number, number, number, number, number, number, number, number, number] = [
            0.213 + c * 0.787 - s * 0.213, 0.715 - c * 0.715 - s * 0.715, 0.072 - c * 0.072 + s * 0.928,
            0.213 - c * 0.213 + s * 0.143, 0.715 + c * 0.285 + s * 0.14, 0.072 - c * 0.072 - s * 0.283,
            0.213 - c * 0.213 - s * 0.787, 0.715 - c * 0.715 + s * 0.715, 0.072 + c * 0.928 + s * 0.072,
          ];
          const [m0, m1, m2, m3, m4, m5, m6, m7, m8] = m;
          [r, g, b] = [m0 * r + m1 * g + m2 * b, m3 * r + m4 * g + m5 * b, m6 * r + m7 * g + m8 * b].map((v) => Math.min(1, Math.max(0, v))) as [number, number, number];
        } else {
          throw new Error(`filter ${name} is not modelled by this test`);
        }
      }
      return [r * 255, g * 255, b * 255];
    };
    const filters: string[] = [];
    for (let n: Element | null = el; n; n = n.parentElement) {
      const f = getComputedStyle(n).filter;
      if (f && f !== 'none') filters.push(f);
    }
    let bg: number[] | null = null;
    for (let n: Element | null = el; n && !bg; n = n.parentElement) {
      const c = parse(getComputedStyle(n).backgroundColor);
      if ((c[3] ?? 0) > 0.95) bg = c;
    }
    bg ??= [255, 255, 255, 1];
    let fg = parse(getComputedStyle(el).color);
    let back = bg;
    for (const f of filters) {
      fg = applyFilter(fg, f);
      back = applyFilter(back, f);
    }
    const lum = (c: number[]) => {
      const [r, g, b] = c.map((v) => {
        const s = v / 255;
        return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
      }) as [number, number, number];
      return 0.2126 * r + 0.7152 * g + 0.0722 * b;
    };
    const [hi, lo] = [lum(fg), lum(back)].sort((a, b) => b - a) as [number, number];
    return { fg: fg.map(Math.round), bg: back.map(Math.round), ratio: (hi + 0.05) / (lo + 0.05) };
  });
}

/** Expands an operation; the page enables "Try it out" for every operation, so its fields are editable at once. */
async function openOperation(page: Page, opId: string): Promise<Locator> {
  const op = page.getByTestId('swagger-host').locator(`#${opId}`);
  await op.locator('.opblock-summary').click();
  await expect(op.getByRole('button', { name: 'Execute' })).toBeVisible();
  return op;
}

for (const scheme of ['dark', 'light'] as const) {
  test(`Swagger UI inputs show what is typed in the ${scheme} theme`, async ({ page }) => {
    const errors = watchConsole(page);
    await page.addInitScript((s) => window.localStorage.setItem('mantine-color-scheme-value', s), scheme);
    await login(page, ADMIN_TOKEN);
    await expect(page.locator('html')).toHaveAttribute('data-mantine-color-scheme', scheme);
    await page.getByRole('link', { name: 'API docs' }).click();
    await expect(page.getByTestId('swagger-host').getByText('FastMock Admin API')).toBeVisible({ timeout: 20_000 });

    const get = await openOperation(page, 'operations-requests-getRequest');
    const input = get.locator('.parameters input').first();
    await input.fill('12345');
    await expect(input).toHaveValue('12345');
    const inputContrast = await seenContrast(input);
    expect(inputContrast.ratio, `parameter input ${JSON.stringify(inputContrast)}`).toBeGreaterThanOrEqual(4.5);

    const post = await openOperation(page, 'operations-stubs-validateStub');
    const body = post.locator('textarea.body-param__text');
    await body.fill('{"request": {"method": "GET"}}');
    const bodyContrast = await seenContrast(body);
    expect(bodyContrast.ratio, `body textarea ${JSON.stringify(bodyContrast)}`).toBeGreaterThanOrEqual(4.5);
    expect(cspViolations(errors)).toEqual([]);
  });
}
