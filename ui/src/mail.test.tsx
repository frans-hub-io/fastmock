import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import { tokenStore } from './api/http';
import type { MailEntry, MailMessage } from './api/mail';
import { visibleNav } from './components/nav';
import {
  addTerm,
  bareAddress,
  composeIssues,
  domainOf,
  loginRule,
  mailDomainOwner,
  mailEndpoints,
  mailServerInfo,
  rawRecipient,
  smtpSnippets,
  isUnread,
  loadSeen,
  mailChaosIssues,
  markAllSeen,
  markSeen,
  ordinal,
  prepareMailHtml,
  rawIssue,
  sourceLabel,
  SseParser,
  validAddress,
  waitSnippet,
} from './lib/mail';
import { draftToRule, EMPTY_DRAFT, ruleToDraft } from './lib/mailChaos';
import { connectMailStream, type StreamEvent, type StreamStatus } from './lib/mailStream';
import { renderApp } from './test/render';

const ADMIN = { name: 'ops', role: 'admin', kind: 'token', permissions: { global: ['*'], spaces: {} } };
const VIEWER = { name: 'dash', role: 'viewer', kind: 'token', permissions: { global: ['stubs:read', 'journal:read', 'mail:read', 'info:read'], spaces: {} } };

function entry(id: string, subject: string, extra: Partial<MailEntry> = {}): MailEntry {
  return {
    id,
    receivedAt: new Date(Date.now() - 60_000).toISOString(),
    source: 'smtp',
    envelope: { mailFrom: 'shop@example.com', rcptTo: ['alice@example.test'], helo: 'client.example.com', clientIp: '198.51.100.7', tls: true, authUser: 'shop' },
    summary: { subject, from: [{ name: 'Shop', address: 'shop@example.com' }], to: [{ address: 'alice@example.test' }], size: 1234, preview: `${subject} preview` },
    labels: [],
    ...extra,
  };
}

const E1 = entry('01a1fb9c-0000-7000-8000-000000000001', 'Your code', { labels: ['otp'] });
const E2 = entry('01a1fb9c-0000-7000-8000-000000000002', 'Invoice', {
  source: 'provider',
  provider: 'sendgrid',
  summary: { subject: 'Invoice', from: [{ address: 'billing@example.com' }], size: 2000, hasAttachments: true, attachmentNames: ['invoice.pdf'] },
});
const E3 = entry('01a1fb9c-0000-7000-8000-000000000003', 'Live arrival');

const MSG: MailMessage = {
  ...E1,
  headers: [
    { name: 'From', value: 'Shop <shop@example.com>' },
    { name: 'To', value: 'alice@example.test' },
    { name: 'Subject', value: 'Your code' },
    { name: 'X-Mailer', value: 'test' },
  ],
  from: [{ name: 'Shop', address: 'shop@example.com' }],
  to: [{ address: 'alice@example.test' }],
  cc: [],
  bcc: [],
  replyTo: [],
  subject: 'Your code',
  date: '2026-11-20T10:00:00Z',
  messageId: 'abc@example.com',
  text: 'Your code is 482913',
  hasHtml: true,
  attachments: [{ partId: '3', name: 'invoice.pdf', contentType: 'application/pdf', size: 2048, inline: false }],
  urls: { raw: '', transcript: '', links: '', codes: '', parts: '' },
};

/** A controllable `text/event-stream` body. */
function sseStream() {
  let ctl: ReadableStreamDefaultController<Uint8Array> | undefined;
  const enc = new TextEncoder();
  const body = new ReadableStream<Uint8Array>({
    start(c) {
      ctl = c;
    },
  });
  return {
    body,
    send: (text: string) => ctl?.enqueue(enc.encode(text)),
    close: () => ctl?.close(),
  };
}

interface Call {
  method: string;
  path: string;
  headers: Headers;
  body: string | null;
}

type Reply = { status?: number; body?: unknown; text?: string; type?: string; stream?: ReadableStream<Uint8Array> } | undefined;

function server(opts: { principal?: unknown; features?: string[]; route?: (method: string, url: URL, body: string | null) => Reply; streams?: ReturnType<typeof sseStream>[] }) {
  const calls: Call[] = [];
  let streamIndex = 0;
  const fn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const req = input instanceof Request ? input : new Request(new URL(String(input), 'http://localhost'), init);
    const url = new URL(req.url, 'http://localhost');
    const body = req.method === 'GET' || req.method === 'HEAD' ? null : await req.text();
    calls.push({ method: req.method, path: url.pathname + url.search, headers: req.headers, body });
    const json = (status: number, b: unknown) => new Response(status === 204 ? null : JSON.stringify(b), { status, headers: { 'content-type': 'application/json' } });
    if (url.pathname === '/__admin/info') return json(200, { name: 'FastMock', version: '0.1.0', storage: 'memory', uptimeSeconds: 1, stubsLoaded: 0, plugins: [], principal: opts.principal ?? ADMIN, authDisabled: false });
    if (url.pathname === '/__admin/version') return json(200, { version: '0.1.0', features: opts.features ?? ['mail', 'mail.imap'] });
    if (url.pathname === '/__admin/mail/stream') {
      const s = opts.streams?.[streamIndex++];
      if (!s) return new Promise<Response>(() => undefined);
      return new Response(s.body, { status: 200, headers: { 'content-type': 'text/event-stream' } });
    }
    const r = opts.route?.(req.method, url, body);
    if (!r) return json(404, { error: 'not found' });
    if (r.text !== undefined) return new Response(r.text, { status: r.status ?? 200, headers: { 'content-type': r.type ?? 'text/plain' } });
    return json(r.status ?? 200, r.body ?? {});
  });
  vi.stubGlobal('fetch', fn);
  return calls;
}

if (!('fonts' in document)) Object.defineProperty(document, 'fonts', { value: { addEventListener: () => undefined, removeEventListener: () => undefined }, configurable: true });

beforeEach(() => {
  tokenStore.set('good');
  window.sessionStorage.clear();
  window.localStorage.clear();
});

afterEach(() => {
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

const TLS_CONFIG = { mailSmtpPort: null, mailSmtpsPort: 4650, mailImapPort: 1143, mailImapsPort: 1993, mailPop3Port: null, mailPop3sPort: 1995, mailHostname: 'mail.example.test', mailPlaintextAuth: false };

describe('mail server settings and domains', () => {
  it('reads the published listeners, host name and login rule', () => {
    expect(mailServerInfo({})).toEqual({ published: false });
    const info = mailServerInfo(TLS_CONFIG);
    expect(info).toEqual({ published: true, hostname: 'mail.example.test', smtps: 4650, imap: 1143, imaps: 1993, pop3s: 1995, plaintextAuth: false });
    expect(mailEndpoints(info, 'imap')).toEqual([
      { label: 'IMAP', port: 1143, tls: false },
      { label: 'IMAPS', port: 1993, tls: true },
    ]);
    expect(mailEndpoints(info, 'smtp')).toEqual([{ label: 'SMTPS', port: 4650, tls: true }]);
    expect(loginRule(info, true, true)).toBe('Plain-text logins are off: on IMAP :1143 the client must start TLS with STARTTLS (IMAP) before logging in, or connect to IMAPS :1993, POP3S :1995.');
    expect(loginRule({ ...info, imap: undefined }, true, true)).toBe('Logins only over TLS: IMAPS :1993, POP3S :1995.');
    expect(loginRule({ ...info, plaintextAuth: true }, true, false)).toMatch(/^Plain-text logins are accepted/);
    expect(loginRule(mailServerInfo({ mailImapPort: 1143 }), true, false)).toBeNull();
    expect(loginRule(info, false, false)).toBeNull();
  });

  it('builds SMTPS snippets with implicit TLS', () => {
    const [swaks, curl, py] = smtpSnippets('mail.example.test', 4650, true);
    expect(swaks?.code).toContain('swaks --server mail.example.test:4650 --tls-on-connect');
    expect(curl?.code).toContain('curl -s smtps://mail.example.test:4650');
    expect(py?.code).toContain('smtplib.SMTP_SSL("mail.example.test", 4650)');
    expect(smtpSnippets('localhost', 2525)[1]?.code).toContain('smtp://localhost:2525');
  });

  it('finds the space that owns a recipient domain like the server does', () => {
    const spaces = [
      { name: 'billing', mailDomains: ['billing.example.test', '*.invoices.example.test'] },
      { name: 'crm', mailDomains: ['crm.example.test'] },
      { name: 'old' },
    ];
    expect(bareAddress('Support <Support@Billing.Example.TEST>')).toBe('support@billing.example.test');
    expect(bareAddress('nobody')).toBeUndefined();
    expect(domainOf('a@billing.example.test.')).toBe('billing.example.test');
    expect(mailDomainOwner('billing.example.test', spaces)).toBe('billing');
    expect(mailDomainOwner('eu.invoices.example.test', spaces)).toBe('billing');
    expect(mailDomainOwner('invoices.example.test', spaces)).toBeNull();
    expect(mailDomainOwner('crm.example.test', spaces)).toBe('crm');
    expect(mailDomainOwner('example.test', spaces)).toBeNull();
    expect(rawRecipient('From: a@example.com\r\nTo: "Support"\r\n <support@crm.example.test>\r\n\r\nTo: x@y.z')).toBe('support@crm.example.test');
    expect(rawRecipient('From: a@example.com\nBcc: b@billing.example.test\n\nHi')).toBe('b@billing.example.test');
    expect(rawRecipient('Subject: none\n\nTo: x@y.z')).toBeUndefined();
  });
});

describe('mail helpers', () => {
  it('parses server-sent events split across chunks, with comments and CRLF', () => {
    const p = new SseParser();
    expect(p.push(': ready\n\nevent: message.rec')).toEqual([{ event: '', data: '', comment: 'ready' }]);
    expect(p.push('eived\r\ndata: {"a":')).toEqual([]);
    expect(p.push('1}\r\n\r\n: heartbeat\n')).toEqual([
      { event: 'message.received', data: '{"a":1}' },
      { event: '', data: '', comment: 'heartbeat' },
    ]);
    expect(p.push('event: resync\ndata: {"missed": 3}\n\n')).toEqual([{ event: 'resync', data: '{"missed": 3}' }]);
  });

  it('prepares HTML for a sandboxed srcdoc frame under the UI policy', () => {
    const src =
      '<html><head><style>.x{color:red;background:url(https://t.example.com/p.gif)}</style><meta http-equiv="refresh" content="0;url=https://example.com"></head>' +
      '<body><p style="color: green" onclick="alert(1)">Hi</p><a href="javascript:alert(1)">x</a><img src="https://t.example.com/a.png"><img src="data:image/png;base64,AA=="><script>alert(1)</script><iframe src="https://example.com"></iframe></body></html>';
    const blocked = prepareMailHtml(src, { nonce: 'n0nce' });
    expect(blocked.remoteImages).toBe(2);
    expect(blocked.html).not.toMatch(/<script|<iframe|onclick|javascript:|http-equiv| src="https:|url\(https/);
    expect(blocked.html).toContain('data-fm-blocked-src="https://t.example.com/a.png"');
    expect(blocked.html).toContain('src="data:image/png;base64,AA=="');
    expect(blocked.html).not.toMatch(/ style=/);
    expect(blocked.html).toMatch(/<style nonce="n0nce">\[data-fm-s="1"\]\[data-fm-s="1"\]\{color: green !important\}<\/style>/);
    expect(blocked.html.match(/<style nonce="n0nce">/g)?.length).toBe(3);
    const allowed = prepareMailHtml(src, { allowImages: true });
    expect(allowed.html).toContain('src="https://t.example.com/a.png"');
    expect(allowed.html).toContain('url(https://t.example.com/p.gif)');
    expect(allowed.html).not.toContain('nonce=');
  });

  it('checks compose forms and raw messages like the server', () => {
    expect(validAddress('Shop <shop@example.com>')).toBe(true);
    expect(validAddress('shop@')).toBe(false);
    expect(composeIssues({ from: '', to: [] })).toMatchObject({ from: 'From is required', to: 'Add at least one recipient', body: expect.any(String) as string });
    expect(composeIssues({ from: 'a@example.com', to: ['nope'], text: 'x' }).to).toBe('Not an address: nope');
    expect(composeIssues({ from: 'a@example.com', to: ['b@example.test'], subject: 'a\nb', text: 'x' }).subject).toMatch(/one line/);
    expect(composeIssues({ from: 'a@example.com', to: ['b@example.test'], text: 'x' })).toEqual({});
    expect(rawIssue('')).toBeDefined();
    expect(rawIssue('hello world\n\nbody')).toMatch(/header fields/);
    expect(rawIssue('From: a@example.com\nSubject: x\n\nbody')).toBeUndefined();
  });

  it('checks mail chaos rules like the server', () => {
    const base = draftToRule(EMPTY_DRAFT);
    expect(mailChaosIssues(base, [])).toEqual({});
    expect(mailChaosIssues({ ...base, id: 'bad id' }, []).id).toBeDefined();
    expect(mailChaosIssues({ ...base, id: 'x' }, [{ ...base, id: 'x' }]).id).toMatch(/twice/);
    expect(mailChaosIssues({ ...base, action: { reply: { code: 451, enhanced: '5.1.1' } } }, []).enhanced).toMatch(/start with 4/);
    expect(mailChaosIssues({ ...base, action: { reply: { code: 250 } } }, []).code).toBeDefined();
    expect(mailChaosIssues({ ...base, action: {} }, []).action).toMatch(/Choose an effect/);
    expect(mailChaosIssues({ ...base, action: { disconnect: true, reply: { code: 451 } } }, []).action).toMatch(/one of/);
    expect(mailChaosIssues({ ...base, stage: 'provider', action: { reply: { code: 451 } } }, []).action).toMatch(/provider/);
    expect(mailChaosIssues({ ...base, action: { providerError: 'rateLimit' } }, []).action).toMatch(/only at stage provider/);
    expect(mailChaosIssues({ ...base, stage: 'ehlo', match: { rcptTo: { contains: 'x' } } }, []).match).toMatch(/does not apply at stage ehlo/);
    expect(mailChaosIssues({ ...base, action: { delay: '11m' } }, []).delay).toBeDefined();
    expect(mailChaosIssues({ ...base, action: { delay: 'soon' } }, []).delay).toBeDefined();
    expect(mailChaosIssues({ ...base, seed: 3 }, []).seed).toBeDefined();
    expect(mailChaosIssues({ ...base, probability: 101 }, []).probability).toBeDefined();
    expect(mailChaosIssues({ ...base, every: 0 }, []).every).toBeDefined();
    expect(mailChaosIssues(base, Array.from({ length: 100 }, () => base)).limit).toBeDefined();
  });

  it('turns rules into form drafts and back', () => {
    const rule = { id: 'r1', stage: 'rcpt' as const, match: { rcptTo: { contains: 'slow@', caseInsensitive: true }, mailFrom: { or: [{ equalTo: 'a' }] } }, action: { delay: '2s' }, every: 3 };
    const d = ruleToDraft(rule);
    expect(d.effect).toBe('delay');
    expect(d.match.mailFrom.custom).toEqual({ or: [{ equalTo: 'a' }] });
    expect(draftToRule(d)).toEqual(rule);
  });

  it('writes a wait snippet, ordinals, search terms and source labels', () => {
    const s = waitSnippet({ ...E1, space: 'billing' }, 'http://127.0.0.1:9090');
    expect(s).toContain("'http://127.0.0.1:9090/__admin/mail/messages/wait?space=billing'");
    expect(s).toContain('"subject":{"equalTo":"Your code"}');
    expect(s).toContain('"to":{"contains":"alice@example.test"}');
    expect([1, 2, 3, 4, 11, 12, 13, 22].map(ordinal)).toEqual(['1st', '2nd', '3rd', '4th', '11th', '12th', '13th', '22nd']);
    expect(addTerm('from:a', 'subject', 'two words')).toBe('from:a subject:"two words"');
    expect(sourceLabel(E2)).toBe('SendGrid');
    expect(sourceLabel(E1)).toBe('SMTP');
  });

  it('remembers read messages in this browser', () => {
    let seen = loadSeen();
    expect(isUnread(seen, E1)).toBe(true);
    seen = markSeen(seen, E1.id);
    expect(isUnread(loadSeen(), E1)).toBe(false);
    expect(isUnread(seen, E2)).toBe(true);
    markAllSeen(Date.now());
    expect(isUnread(loadSeen(), E2)).toBe(false);
  });
});

describe('mail stream', () => {
  it('follows the stream with the bearer header and reports events', async () => {
    const s = sseStream();
    const calls = server({ streams: [s] });
    const events: StreamEvent[] = [];
    const statuses: StreamStatus[] = [];
    const close = connectMailStream({ space: 'billing', onEvent: (e) => events.push(e), onStatus: (x) => statuses.push(x) });
    s.send(': ready\n\n');
    s.send(`event: message.received\ndata: ${JSON.stringify({ type: 'message.received', ids: [E3.id], entry: E3 })}\n\n`);
    s.send('event: message.deleted\ndata: {"type":"message.deleted","ids":["x"]}\n\nevent: resync\ndata: {"missed":2}\n\n');
    await waitFor(() => expect(events).toHaveLength(3));
    expect(events.map((e) => e.type)).toEqual(['message.received', 'message.deleted', 'resync']);
    expect(statuses).toEqual(['connecting', 'live']);
    const call = calls.find((c) => c.path.startsWith('/__admin/mail/stream'));
    expect(call?.path).toBe('/__admin/mail/stream?space=billing');
    expect(call?.headers.get('authorization')).toBe('Bearer good');
    expect(call?.headers.get('accept')).toBe('text/event-stream');
    close();
  });

  it('reconnects after the stream ends and asks for a resync', async () => {
    const a = sseStream();
    const b = sseStream();
    server({ streams: [a, b] });
    const events: StreamEvent[] = [];
    const statuses: StreamStatus[] = [];
    const close = connectMailStream({ onEvent: (e) => events.push(e), onStatus: (x) => statuses.push(x), backoff: () => 5 });
    a.send(': ready\n\n');
    a.close();
    await waitFor(() => expect(statuses).toContain('reconnecting'));
    b.send(': ready\n\n');
    await waitFor(() => expect(events).toEqual([{ type: 'resync' }]));
    expect(statuses.at(-1)).toBe('live');
    close();
  });

  it('stops for good when the caller may not read mail', async () => {
    const fn = vi.fn(() => Promise.resolve(new Response(JSON.stringify({ error: 'forbidden' }), { status: 403 })));
    vi.stubGlobal('fetch', fn);
    const statuses: StreamStatus[] = [];
    connectMailStream({ onEvent: () => undefined, onStatus: (x) => statuses.push(x), backoff: () => 1 });
    await waitFor(() => expect(statuses.at(-1)).toBe('off'));
    await new Promise((r) => setTimeout(r, 30));
    expect(fn).toHaveBeenCalledTimes(1);
  });
});

function listRoute(pages: Record<string, { messages: MailEntry[]; nextCursor?: string }>) {
  return (method: string, url: URL): Reply => {
    if (url.pathname === '/__admin/mail/messages' && method === 'GET') return { body: pages[url.searchParams.get('cursor') ?? ''] ?? { messages: [] } };
    return undefined;
  };
}

describe('mail list', () => {
  it('lists messages with badges and pages with the cursor', async () => {
    const calls = server({ route: listRoute({ '': { messages: [E1], nextCursor: 'c2' }, c2: { messages: [E2] } }) });
    renderApp(<AppRoutes />, '/mail');
    const rows = await screen.findAllByTestId('mail-row');
    expect(rows).toHaveLength(1);
    expect(rows[0]).toHaveTextContent('Shop');
    expect(rows[0]).toHaveTextContent('Your code');
    expect(within(rows[0] as HTMLElement).getByTestId('mail-label')).toHaveTextContent('otp');
    expect(rows[0]).toHaveAttribute('data-unread', 'true');
    await userEvent.click(screen.getByTestId('mail-load-more'));
    await waitFor(() => expect(screen.getAllByTestId('mail-row')).toHaveLength(2));
    const second = screen.getAllByTestId('mail-row')[1] as HTMLElement;
    expect(within(second).getByTestId('mail-source')).toHaveTextContent('SendGrid');
    expect(within(second).getByTestId('mail-attachment-icon')).toBeInTheDocument();
    expect(calls.some((c) => c.path === '/__admin/mail/messages?cursor=c2&limit=50')).toBe(true);
  });

  it('searches with the query syntax and offers its help', async () => {
    const calls = server({ route: listRoute({ '': { messages: [E1] } }) });
    renderApp(<AppRoutes />, '/mail');
    await screen.findAllByTestId('mail-row');
    await userEvent.click(screen.getByTestId('mail-query-help'));
    await userEvent.click(await screen.findByText('has:attachment'));
    await waitFor(() => expect(calls.some((c) => c.path.includes('query=has%3Aattachment'))).toBe(true));
    const input = screen.getByTestId('mail-search');
    await userEvent.clear(input);
    await userEvent.type(input, 'from:billing{Enter}');
    await waitFor(() => expect(calls.some((c) => c.path.includes('query=from%3Abilling'))).toBe(true));
  });

  it('adds live messages, removes deleted ones and reloads on resync', async () => {
    const s = sseStream();
    const calls = server({ streams: [s], route: listRoute({ '': { messages: [E1, E2] } }) });
    renderApp(<AppRoutes />, '/mail');
    await waitFor(() => expect(screen.getAllByTestId('mail-row')).toHaveLength(2));
    s.send(': ready\n\n');
    await waitFor(() => expect(screen.getByTestId('mail-live')).toHaveAttribute('data-status', 'live'));
    s.send(`event: message.received\ndata: ${JSON.stringify({ type: 'message.received', ids: [E3.id], entry: E3 })}\n\n`);
    await waitFor(() => expect(screen.getAllByTestId('mail-row')).toHaveLength(3));
    expect(screen.getAllByTestId('mail-row')[0]).toHaveTextContent('Live arrival');
    expect(screen.getAllByTestId('mail-row')[0]).toHaveAttribute('data-fresh', 'true');
    s.send(`event: message.deleted\ndata: ${JSON.stringify({ type: 'message.deleted', ids: [E1.id] })}\n\n`);
    await waitFor(() => expect(screen.queryByText('Your code')).not.toBeInTheDocument());
    const before = calls.filter((c) => c.path.startsWith('/__admin/mail/messages?')).length;
    s.send('event: resync\ndata: {"missed":4}\n\n');
    await waitFor(() => expect(calls.filter((c) => c.path.startsWith('/__admin/mail/messages?')).length).toBeGreaterThan(before));
  });

  it('shows a ready-to-copy SMTP snippet when there is no mail', async () => {
    server({ route: listRoute({}) });
    renderApp(<AppRoutes />, '/mail');
    expect(await screen.findByTestId('mail-empty')).toHaveTextContent('No mail yet');
    expect(screen.getByTestId('smtp-snippet-swaks')).toHaveTextContent('swaks --server localhost:2525');
    expect(screen.getByTestId('mail-empty-smtp')).toHaveTextContent('the FastMock mail Service or host, port 2525, the default');
    expect(screen.getByRole('link', { name: /Mail guide/ })).toHaveAttribute('href', 'https://fastmock.franshub.io/guide/mail/');
  });

  it('points the SMTP snippets at the published host and SMTPS port', async () => {
    server({
      route: (method, url) => {
        if (url.pathname === '/ui/config.json') return { body: TLS_CONFIG };
        return listRoute({})(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail');
    await waitFor(() => expect(screen.getByTestId('smtp-snippet-swaks')).toHaveTextContent('swaks --server mail.example.test:4650 --tls-on-connect'));
    expect(screen.getByTestId('mail-empty-smtp')).toHaveTextContent('on mail.example.test, SMTPS :4650 (TLS)');
  });

  it('says so when no SMTP listener is on', async () => {
    server({
      route: (method, url) => {
        if (url.pathname === '/ui/config.json') return { body: { ...TLS_CONFIG, mailSmtpsPort: null } };
        return listRoute({})(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail');
    await waitFor(() => expect(screen.getByTestId('mail-empty-smtp')).toHaveTextContent('SMTP listeners are off'));
    expect(screen.queryByTestId('smtp-snippet-swaks')).not.toBeInTheDocument();
  });

  it('deletes the messages matching the search after confirmation', async () => {
    const calls = server({
      route: (method, url) => {
        if (method === 'DELETE' && url.pathname === '/__admin/mail/messages') return { body: { deleted: 1, ids: [E1.id] } };
        return listRoute({ '': { messages: [E1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail?q=label%3Aotp');
    await screen.findAllByTestId('mail-row');
    await userEvent.click(screen.getByTestId('mail-delete-matching'));
    await userEvent.click(await screen.findByTestId('confirm-button'));
    await waitFor(() => expect(calls.some((c) => c.method === 'DELETE' && c.path === '/__admin/mail/messages?query=label%3Aotp')).toBe(true));
  });
});

describe('message view', () => {
  function messageServer() {
    return server({
      route: (method, url) => {
        const base = `/__admin/mail/messages/${MSG.id}`;
        if (url.pathname === base && method === 'GET') return { body: MSG };
        if (url.pathname === `${base}/html`) return { text: '<p style="color:red">Hello</p><script>parent.hacked = true</script>', type: 'text/html' };
        if (url.pathname === `${base}/raw`) return { text: 'From: Shop <shop@example.com>\r\nSubject: Your code\r\n\r\nYour code is 482913', type: 'message/rfc822' };
        if (url.pathname === `${base}/transcript`) return { body: { transcript: [{ at: '2026-11-20T10:00:00Z', direction: 'server', text: '220 ready' }, { at: '2026-11-20T10:00:00.010Z', direction: 'client', text: 'EHLO client' }] } };
        if (url.pathname === `${base}/links`) return { body: { links: [{ url: 'https://example.com/verify', text: 'Verify' }] } };
        if (url.pathname === `${base}/codes`) return url.searchParams.get('pattern') === '(' ? { status: 400, body: { error: 'invalid pattern' } } : { body: { codes: ['482913'], pattern: '\\b\\d{4,8}\\b' } };
        if (url.pathname === `${base}/labels` && method === 'PUT') return { body: MSG };
        return undefined;
      },
    });
  }

  it('shows the header block and renders HTML in a sandboxed frame without scripts', async () => {
    messageServer();
    renderApp(<AppRoutes />, `/mail/${MSG.id}`);
    expect(await screen.findByRole('heading', { name: 'Your code' })).toBeInTheDocument();
    expect(screen.getByTestId('mail-header-block')).toHaveTextContent('Shop <shop@example.com>');
    expect(screen.getByTestId('mail-envelope')).toHaveTextContent('198.51.100.7');
    expect(screen.getByTestId('mail-envelope')).toHaveTextContent('TLS');
    const frame = await screen.findByTestId('mail-html-frame');
    expect(frame).toHaveAttribute('sandbox', '');
    const doc = frame.getAttribute('srcdoc') ?? '';
    expect(doc).toContain('Hello');
    expect(doc).not.toContain('<script');
    expect(isUnread(loadSeen(), MSG)).toBe(false);
  });

  it('switches between the text, headers, raw, attachments, transcript and links tabs', async () => {
    const calls = messageServer();
    renderApp(<AppRoutes />, `/mail/${MSG.id}`);
    await screen.findByRole('heading', { name: 'Your code' });
    await userEvent.click(screen.getByTestId('mail-tab-text'));
    expect(await screen.findByTestId('mail-text')).toHaveTextContent('Your code is 482913');
    await userEvent.click(screen.getByTestId('mail-tab-headers'));
    const headers = await screen.findByTestId('mail-headers');
    expect(within(headers).getAllByRole('row')).toHaveLength(5);
    expect(headers).toHaveTextContent('X-Mailer');
    await userEvent.click(screen.getByTestId('mail-tab-raw'));
    expect(await screen.findByTestId('mail-raw')).toHaveTextContent('Subject: Your code');
    await userEvent.click(screen.getByTestId('mail-tab-attachments'));
    expect(await screen.findByTestId('mail-attachments')).toHaveTextContent('invoice.pdf');
    await userEvent.click(screen.getByTestId('mail-tab-transcript'));
    expect(await screen.findByTestId('mail-transcript')).toHaveTextContent('EHLO client');
    await userEvent.click(screen.getByTestId('mail-tab-links'));
    expect(await screen.findByTestId('mail-code')).toHaveTextContent('482913');
    expect(screen.getByTestId('mail-link')).toHaveTextContent('https://example.com/verify');
    fireEvent.change(screen.getByTestId('code-pattern'), { target: { value: '(' } });
    await waitFor(() => expect(calls.some((c) => c.path.endsWith('/codes?pattern=%28'))).toBe(true));
    expect(await screen.findByText('invalid pattern')).toBeInTheDocument();
    const raw = calls.find((c) => c.path.endsWith('/raw'));
    expect(raw?.headers.get('authorization')).toBe('Bearer good');
  });

  it('offers a wait snippet for tests', async () => {
    messageServer();
    renderApp(<AppRoutes />, `/mail/${MSG.id}`);
    await userEvent.click(await screen.findByTestId('mail-test-snippet'));
    expect(await screen.findByTestId('mail-wait-snippet')).toHaveTextContent('/__admin/mail/messages/wait');
  });

  it('saves labels', async () => {
    const calls = messageServer();
    renderApp(<AppRoutes />, `/mail/${MSG.id}`);
    const input = await screen.findByTestId('mail-labels-input');
    await userEvent.type(input, 'signup{Enter}');
    await userEvent.click(await screen.findByTestId('mail-labels-save'));
    await waitFor(() => expect(calls.find((c) => c.method === 'PUT')?.body).toBe('{"labels":["otp","signup"]}'));
  });
});

describe('inject dialog', () => {
  it('validates the compose form and posts the message', async () => {
    const calls = server({
      route: (method, url) => {
        if (method === 'POST' && url.pathname === '/__admin/mail/messages') return { body: { messages: [E1] } };
        return listRoute({ '': { messages: [E1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail');
    await userEvent.click(await screen.findByTestId('mail-inject'));
    const dialog = await screen.findByRole('dialog', { name: 'Inject a message' });
    await userEvent.click(within(dialog).getByTestId('inject-submit'));
    expect(await within(dialog).findByText('From is required')).toBeInTheDocument();
    expect(within(dialog).getByText('Add at least one recipient')).toBeInTheDocument();
    expect(within(dialog).getByTestId('inject-body-issue')).toBeInTheDocument();
    expect(calls.some((c) => c.method === 'POST')).toBe(false);
    await userEvent.type(within(dialog).getByTestId('inject-from'), 'not-an-address');
    expect(await within(dialog).findByText(/Use addr@example.com/)).toBeInTheDocument();
    await userEvent.clear(within(dialog).getByTestId('inject-from'));
    await userEvent.type(within(dialog).getByTestId('inject-from'), 'Shop <shop@example.com>');
    await userEvent.type(within(dialog).getByTestId('inject-to'), 'alice@example.test{Enter}');
    await userEvent.type(within(dialog).getByTestId('inject-subject'), 'Hello');
    await userEvent.type(within(dialog).getByTestId('inject-text'), 'Body');
    await userEvent.click(within(dialog).getByTestId('inject-submit'));
    await waitFor(() => expect(calls.find((c) => c.method === 'POST')?.body).toBe(JSON.stringify({ from: 'Shop <shop@example.com>', to: ['alice@example.test'], subject: 'Hello', text: 'Body' })));
  });

  it('stores the message in the space that owns the recipient domain unless another space is picked', async () => {
    const SPACES = [
      { name: 'billing', pathPrefixes: [], hosts: [], mailDomains: ['billing.example.test'], permissions: ['mail:read', 'mail:write'] },
      { name: 'crm', pathPrefixes: [], hosts: [], mailDomains: ['crm.example.test'], permissions: ['mail:read', 'mail:write'] },
    ];
    const calls = server({
      route: (method, url) => {
        if (url.pathname === '/__admin/spaces') return { body: { spaces: SPACES } };
        if (method === 'POST' && url.pathname === '/__admin/mail/messages') return { body: { messages: [E1] } };
        return listRoute({ '': { messages: [E1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail?inject=1');
    const dialog = await screen.findByRole('dialog', { name: 'Inject a message' });
    const field = await within(dialog).findByTestId('space-field');
    expect(field).toHaveValue('Global area (no space)');
    await userEvent.type(within(dialog).getByTestId('inject-to'), 'support@billing.example.test{Enter}');
    await waitFor(() => expect(field).toHaveValue('billing'));
    expect(within(dialog).queryByTestId('inject-space-hint')).not.toBeInTheDocument();
    await userEvent.type(within(dialog).getByTestId('inject-to'), 'x@crm.example.test{Enter}');
    expect(field).toHaveValue('billing');
    await userEvent.type(within(dialog).getByTestId('inject-from'), 'shop@example.com');
    await userEvent.type(within(dialog).getByTestId('inject-text'), 'Body');
    await userEvent.click(within(dialog).getByTestId('inject-submit'));
    await waitFor(() => expect(calls.find((c) => c.method === 'POST')?.path).toBe('/__admin/mail/messages?space=billing'));
  });

  it('keeps a space picked by hand and says where logins look', async () => {
    const SPACES = [{ name: 'billing', pathPrefixes: [], hosts: [], mailDomains: ['billing.example.test'], permissions: ['mail:read', 'mail:write'] }];
    server({
      route: (method, url) => {
        if (url.pathname === '/__admin/spaces') return { body: { spaces: SPACES } };
        return listRoute({ '': { messages: [E1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail?inject=1');
    const dialog = await screen.findByRole('dialog', { name: 'Inject a message' });
    const field = await within(dialog).findByTestId('space-field');
    await userEvent.type(within(dialog).getByTestId('inject-to'), 'support@billing.example.test{Enter}');
    await waitFor(() => expect(field).toHaveValue('billing'));
    await userEvent.click(field);
    await userEvent.click(await screen.findByRole('option', { name: 'Global area (no space)', hidden: true }));
    expect(field).toHaveValue('Global area (no space)');
    expect(within(dialog).getByTestId('inject-space-hint')).toHaveTextContent('billing.example.test belongs to billing: IMAP and POP3 logins as this address see only billing.');
    await userEvent.type(within(dialog).getByTestId('inject-to'), '{Backspace}{Backspace}bob@billing.example.test{Enter}');
    expect(field).toHaveValue('Global area (no space)');
  });

  it('keeps the selected space for a domain no space owns, with the login to use', async () => {
    window.localStorage.setItem('fastmock.space', 'billing');
    const SPACES = [{ name: 'billing', pathPrefixes: [], hosts: [], mailDomains: ['billing.example.test'], permissions: ['mail:read', 'mail:write'] }];
    server({
      route: (method, url) => {
        if (url.pathname === '/__admin/spaces') return { body: { spaces: SPACES } };
        return listRoute({ '': { messages: [E1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail?inject=1');
    const dialog = await screen.findByRole('dialog', { name: 'Inject a message' });
    const field = await within(dialog).findByTestId('space-field');
    expect(field).toHaveValue('billing');
    await userEvent.type(within(dialog).getByTestId('inject-to'), 'alice@example.test{Enter}');
    expect(field).toHaveValue('billing');
    expect(within(dialog).getByTestId('inject-space-hint')).toHaveTextContent('No space owns example.test: IMAP and POP3 log in to this copy as billing/alice@example.test.');
  });

  it('posts a raw message as message/rfc822', async () => {
    const calls = server({
      route: (method, url) => {
        if (method === 'POST' && url.pathname === '/__admin/mail/messages') return { body: { messages: [E1] } };
        return listRoute({ '': { messages: [E1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/mail?inject=1');
    const dialog = await screen.findByRole('dialog', { name: 'Inject a message' });
    await userEvent.click(within(dialog).getByText('Raw message (EML)'));
    fireEvent.change(within(dialog).getByTestId('inject-raw'), { target: { value: 'not a message' } });
    await userEvent.click(within(dialog).getByTestId('inject-submit'));
    expect(await within(dialog).findByText(/must start with header fields/)).toBeInTheDocument();
    fireEvent.change(within(dialog).getByTestId('inject-raw'), { target: { value: 'From: a@example.com\nTo: b@example.test\n\nHi' } });
    await userEvent.click(within(dialog).getByTestId('inject-submit'));
    await waitFor(() => expect(calls.find((c) => c.method === 'POST')?.body).toBe('From: a@example.com\r\nTo: b@example.test\r\n\r\nHi'));
    expect(calls.find((c) => c.method === 'POST')?.headers.get('content-type')).toBe('message/rfc822');
  });
});

describe('mail chaos', () => {
  const view = { rules: [{ id: 'third-fails', stage: 'data', every: 3, times: 1, action: { reply: { code: 451, enhanced: '4.3.0', text: 'Try again later' } } }], counters: [{ id: 'third-fails', matched: 7, fired: 1 }] };

  it('lists rules with counters, validates the form and saves', async () => {
    const calls = server({
      route: (method, url) => {
        if (url.pathname === '/__admin/mail/chaos' && method === 'GET') return { body: view };
        if (url.pathname === '/__admin/mail/chaos' && method === 'PUT') return { body: view };
        return undefined;
      },
    });
    renderApp(<AppRoutes />, '/mail/chaos');
    const row = await screen.findByTestId('mail-chaos-row');
    expect(row).toHaveTextContent('third-fails');
    expect(row).toHaveTextContent('451 4.3.0 Try again later');
    expect(row).toHaveTextContent('every 3rd · at most 1×');
    expect(within(row).getByTestId('mail-chaos-counter')).toHaveTextContent('7 / 1');
    const enhanced = screen.getByTestId('chaos-enhanced');
    await userEvent.clear(enhanced);
    await userEvent.type(enhanced, '5.1.1');
    await userEvent.click(screen.getByTestId('chaos-add'));
    expect(await screen.findByText('Must start with 4 like the code')).toBeInTheDocument();
    expect(screen.getAllByTestId('mail-chaos-row')).toHaveLength(1);
    await userEvent.clear(enhanced);
    await userEvent.type(screen.getByTestId('chaos-id'), 'slow');
    await userEvent.type(screen.getByTestId('chaos-match-rcptTo'), 'slow@');
    await userEvent.click(screen.getByTestId('chaos-add'));
    await waitFor(() => expect(screen.getAllByTestId('mail-chaos-row')).toHaveLength(2));
    await userEvent.click(screen.getByTestId('save-mail-chaos'));
    await waitFor(() => expect(calls.some((c) => c.method === 'PUT')).toBe(true));
    const body = JSON.parse(calls.find((c) => c.method === 'PUT')?.body ?? '{}') as { rules: unknown[] };
    expect(body.rules[1]).toEqual({ id: 'slow', stage: 'rcpt', match: { rcptTo: { contains: 'slow@' } }, action: { reply: { code: 451 } } });
  });

  it('refuses a duplicate id before saving', async () => {
    server({ route: (_method, url) => (url.pathname === '/__admin/mail/chaos' ? { body: view } : undefined) });
    renderApp(<AppRoutes />, '/mail/chaos');
    await screen.findByTestId('mail-chaos-row');
    await userEvent.type(screen.getByTestId('chaos-id'), 'third-fails');
    expect(await screen.findByText('Id third-fails is used twice')).toBeInTheDocument();
  });
});

describe('mail permissions and feature gating', () => {
  it('shows Mail in the navigation only when the server runs the mail module', () => {
    const perms = { global: ['mail:read'], spaces: {} };
    const items = (f: string[]) => visibleNav(perms, false, f).flatMap((g) => g.items.map((i) => i.label));
    expect(items(['mail'])).toContain('Mail');
    expect(items([])).not.toContain('Mail');
    expect(visibleNav({ global: ['stubs:read'], spaces: {} }, false, ['mail']).flatMap((g) => g.items.map((i) => i.label))).not.toContain('Mail');
  });

  it('says how to enable mail when the module is off', async () => {
    server({ features: ['tokens'] });
    renderApp(<AppRoutes />, '/mail');
    expect(await screen.findByTestId('mail-off')).toHaveTextContent('FASTMOCK_MAIL_ENABLED=true');
  });

  it('gives a reader no write or delete controls', async () => {
    server({ principal: VIEWER, route: listRoute({ '': { messages: [E1] } }) });
    renderApp(<AppRoutes />, '/mail');
    await screen.findAllByTestId('mail-row');
    expect(screen.queryByTestId('mail-inject')).not.toBeInTheDocument();
    expect(screen.queryByTestId('mail-delete-matching')).not.toBeInTheDocument();
  });

  it('shows chaos rules read-only and mailbox passwords disabled for a reader', async () => {
    server({
      principal: VIEWER,
      route: (_method, url) => {
        if (url.pathname === '/__admin/mail/chaos') return { body: { rules: [{ id: 'r', stage: 'rcpt', action: { delay: '1s' } }], counters: [] } };
        if (url.pathname === '/__admin/mail/mailboxes') return { body: { mailboxes: [{ address: 'alice@example.test', folders: ['INBOX'], hasPassword: false }] } };
        return undefined;
      },
    });
    renderApp(<AppRoutes />, '/mail/chaos');
    await screen.findByTestId('mail-chaos-row');
    expect(screen.queryByTestId('mail-chaos-editor')).not.toBeInTheDocument();
    expect(screen.queryByTestId('save-mail-chaos')).not.toBeInTheDocument();
    await userEvent.click(screen.getByTestId('mail-section-mailboxes'));
    const row = await screen.findByTestId('mailbox-row');
    expect(within(row).getByTestId('missing-permission')).toHaveAttribute('data-permission', 'mail:write');
  });

  it('shows the published mail host, every IMAP and POP3 port and the TLS login rule', async () => {
    server({
      features: ['mail', 'mail.imap', 'mail.pop3'],
      route: (_method, url) => {
        if (url.pathname === '/ui/config.json') return { body: TLS_CONFIG };
        if (url.pathname === '/__admin/mail/mailboxes') return { body: { mailboxes: [{ address: 'support@billing.example.test', folders: ['INBOX'], hasPassword: false }] } };
        return undefined;
      },
    });
    renderApp(<AppRoutes />, '/mail/mailboxes');
    const help = await screen.findByTestId('mailbox-help');
    await waitFor(() => expect(within(help).getByTestId('mailbox-help-host')).toHaveTextContent('mail.example.test'));
    expect(within(help).getByTestId('mail-endpoint-imap')).toHaveTextContent('IMAP :1143');
    expect(within(help).getByTestId('mail-endpoint-imaps')).toHaveTextContent('IMAPS :1993 · TLS');
    expect(within(help).getByTestId('mail-endpoint-pop3s')).toHaveTextContent('POP3S :1995 · TLS');
    expect(within(help).queryByTestId('mail-endpoint-pop3')).not.toBeInTheDocument();
    expect(within(help).getByTestId('mailbox-help-logins')).toHaveTextContent('STARTTLS (IMAP)');
    const address = within(await screen.findByTestId('mailbox-row')).getByTitle('support@billing.example.test');
    expect(address).toHaveTextContent('support@billing.example.test');
    expect(address.querySelector('wbr')).not.toBeNull();
  });

  it('names no host when the server does not publish one', async () => {
    server({
      route: (_method, url) => {
        if (url.pathname === '/__admin/mail/mailboxes') return { body: { mailboxes: [] } };
        return undefined;
      },
    });
    renderApp(<AppRoutes />, '/mail/mailboxes');
    const help = await screen.findByTestId('mailbox-help');
    expect(within(help).getByTestId('mailbox-help-host')).toHaveTextContent('the FastMock mail Service or host');
    expect(within(help).queryByTestId('mailbox-help-logins')).not.toBeInTheDocument();
  });

  it('sets a mailbox password and shows it once', async () => {
    const calls = server({
      route: (method, url) => {
        if (url.pathname === '/__admin/mail/mailboxes') return { body: { mailboxes: [{ address: 'alice@example.test', folders: ['INBOX'], hasPassword: false }] } };
        if (method === 'PUT' && url.pathname.endsWith('/password')) return { status: 204 };
        return undefined;
      },
    });
    renderApp(<AppRoutes />, '/mail/mailboxes');
    await userEvent.click(await screen.findByRole('button', { name: 'Set the password of alice@example.test' }));
    const dialog = await screen.findByRole('dialog');
    await userEvent.click(within(dialog).getByTestId('mailbox-password-submit'));
    const value = await within(dialog).findByTestId('mailbox-password-value');
    const put = calls.find((c) => c.method === 'PUT');
    expect(put?.path).toBe('/__admin/mail/mailboxes/alice%40example.test/password');
    expect(JSON.parse(put?.body ?? '{}')).toEqual({ password: value.textContent });
    expect(value.textContent).toHaveLength(20);
  });
});
