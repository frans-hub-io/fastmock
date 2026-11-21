import { createConnection } from 'node:net';
import { readFile } from 'node:fs/promises';
import { expect, test } from '@playwright/test';
import { ADMIN_TOKEN, adminApi, configured, cspViolations, login, watchConsole } from './helpers';

/** `host:port` of the server's SMTP listener; the server runs with FASTMOCK_MAIL_ENABLED=true. */
const SMTP = process.env.FASTMOCK_E2E_SMTP ?? '';

test.skip(!configured || !SMTP, 'set FASTMOCK_E2E_ADMIN_TOKEN, FASTMOCK_E2E_VIEWER_TOKEN and FASTMOCK_E2E_SMTP (mail enabled)');

/** Sends one message over SMTP and resolves after the server accepted it. */
function sendSmtp(from: string, to: string, data: string): Promise<void> {
  const [host = '127.0.0.1', port = '2525'] = SMTP.split(':');
  return new Promise((resolve, reject) => {
    const sock = createConnection({ host, port: Number(port) });
    const steps = [`EHLO e2e.example.com`, `MAIL FROM:<${from}>`, `RCPT TO:<${to}>`, 'DATA', `${data.replace(/\r?\n/g, '\r\n').replace(/^\./gm, '..')}\r\n.`, 'QUIT'];
    const expected = [220, 250, 250, 250, 354, 250, 221];
    let buf = '';
    let i = 0;
    sock.setEncoding('utf8');
    sock.on('data', (chunk: string) => {
      buf += chunk;
      for (;;) {
        const m = /^(\d{3}) .*\r\n/m.exec(buf);
        if (!m) return;
        buf = buf.slice(m.index + m[0].length);
        const code = Number(m[1]);
        if (code !== expected[i]) {
          sock.destroy();
          reject(new Error(`SMTP step ${String(i)} answered ${m[0].trim()}`));
          return;
        }
        const next = steps[i];
        i += 1;
        if (next === undefined) {
          sock.end();
          resolve();
          return;
        }
        sock.write(`${next}\r\n`);
      }
    });
    sock.on('error', reject);
  });
}

const tag = Date.now().toString(36);
const subject = `E2E mail ${tag}`;

test.afterAll(async () => {
  await adminApi(`/__admin/mail/messages?query=${encodeURIComponent(`subject:"${subject}"`)}`, { method: 'DELETE' });
});

test('a message sent over SMTP appears live, renders sandboxed and downloads raw', async ({ page }) => {
  const errors = watchConsole(page);
  await login(page, ADMIN_TOKEN);
  await page.getByRole('link', { name: 'Mail' }).first().click();
  await expect(page).toHaveURL(/\/ui\/mail$/);
  await expect(page.getByTestId('mail-live')).toHaveAttribute('data-status', 'live');

  const html = `<html><head><style>.hi{color:rgb(0, 128, 0)}</style></head><body><p class="hi" id="greeting">Hello from ${tag}</p><p style="color: rgb(200, 0, 0)" id="inline">styled</p><a href="https://example.com/verify?t=${tag}">Verify</a><script>document.body.setAttribute('data-pwned', '1'); parent.document.title = 'pwned';</script></body></html>`;
  const data = [
    'From: E2E Sender <sender@example.com>',
    `To: e2e+inbox@example.test`,
    `Subject: ${subject}`,
    'MIME-Version: 1.0',
    'Content-Type: multipart/alternative; boundary="b1"',
    '',
    '--b1',
    'Content-Type: text/plain; charset=utf-8',
    '',
    `Your code is 731942. Hello from ${tag}`,
    '--b1',
    'Content-Type: text/html; charset=utf-8',
    '',
    html,
    '--b1--',
  ].join('\n');
  await sendSmtp('sender@example.com', 'e2e+inbox@example.test', data);

  const row = page.getByTestId('mail-row').filter({ hasText: subject });
  await expect(row).toBeVisible();
  await expect(row).toHaveAttribute('data-unread', 'true');
  await expect(row.getByTestId('mail-label')).toHaveText('inbox');
  await row.click();

  await expect(page.getByRole('heading', { name: subject })).toBeVisible();
  await expect(page.getByTestId('mail-envelope')).toContainText('e2e+inbox@example.test');
  const frame = page.getByTestId('mail-html-frame');
  await expect(frame).toHaveAttribute('sandbox', '');
  const doc = page.frameLocator('[data-testid="mail-html-frame"]');
  await expect(doc.locator('#greeting')).toHaveText(`Hello from ${tag}`);
  await expect(doc.locator('#greeting')).toHaveCSS('color', 'rgb(0, 128, 0)');
  await expect(doc.locator('#inline')).toHaveCSS('color', 'rgb(200, 0, 0)');
  await expect(doc.locator('body')).not.toHaveAttribute('data-pwned', '1');
  expect(await page.title()).not.toBe('pwned');

  await page.getByTestId('mail-tab-links').click();
  await expect(page.getByTestId('mail-code').first()).toContainText('731942');
  await expect(page.getByTestId('mail-link').first()).toContainText(`https://example.com/verify?t=${tag}`);

  await page.getByTestId('mail-tab-transcript').click();
  await expect(page.getByTestId('mail-transcript')).toContainText('MAIL FROM:<sender@example.com>');

  await page.getByTestId('mail-tab-raw').click();
  await expect(page.getByTestId('mail-raw')).toContainText(`Subject: ${subject}`);
  const [download] = await Promise.all([page.waitForEvent('download'), page.getByTestId('download-raw').click()]);
  expect(download.suggestedFilename()).toMatch(/^[0-9a-f-]{36}\.eml$/);
  const path = await download.path();
  expect(await readFile(path, 'utf8')).toContain(`Subject: ${subject}`);

  await page.getByRole('link', { name: 'Mail', exact: true }).first().click();
  await expect(page.getByTestId('mail-row').filter({ hasText: subject })).not.toHaveAttribute('data-unread', 'true');
  expect(cspViolations(errors)).toEqual([]);
});

test('an injected message can be deleted from its view', async ({ page }) => {
  await login(page, ADMIN_TOKEN);
  await page.goto('/ui/mail?inject=1');
  const dialog = page.getByRole('dialog', { name: 'Inject a message' });
  await dialog.getByTestId('inject-from').fill('QA <qa@example.com>');
  await dialog.getByTestId('inject-to').fill('dave@example.test');
  await dialog.getByTestId('inject-to').press('Enter');
  await dialog.getByTestId('inject-subject').fill(`${subject} injected`);
  await dialog.getByTestId('inject-text').fill('Welcome aboard');
  await dialog.getByTestId('inject-submit').click();
  const row = page.getByTestId('mail-row').filter({ hasText: `${subject} injected` });
  await expect(row).toBeVisible();
  await expect(row.getByTestId('mail-source')).toHaveText('inject');
  await row.click();
  await page.getByTestId('mail-delete').click();
  await page.getByTestId('confirm-button').click();
  await expect(page).toHaveURL(/\/ui\/mail$/);
  await expect(page.getByTestId('mail-row').filter({ hasText: `${subject} injected` })).toHaveCount(0);
});
