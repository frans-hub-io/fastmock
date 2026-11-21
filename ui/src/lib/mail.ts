import type { ChaosStage, ComposeRequest, MailAddress, MailChaosRule, MailEntry, MailStringMatcher } from '../api/mail';
import { shellQuote } from './curl';
import { readLocal, writeLocal } from './ui';

/** `Name <address>`, or the address alone. */
export function addressText(a: MailAddress | undefined | null): string {
  if (!a) return '';
  return a.name ? `${a.name} <${a.address}>` : a.address;
}

/** The name when there is one, else the address: what a list shows of a sender. */
export function addressShort(a: MailAddress | undefined | null): string {
  if (!a) return '';
  return a.name?.trim() || a.address;
}

export function addressList(list: readonly MailAddress[] | undefined | null): string {
  return (list ?? []).map(addressText).join(', ');
}

/** The sender a list shows: the From header, else the envelope sender, else the null sender. */
export function senderOf(e: MailEntry): string {
  const f = e.summary.from?.[0];
  if (f) return addressShort(f);
  return e.envelope.mailFrom || '<>';
}

export function recipientsOf(e: MailEntry): string {
  const to = e.summary.to ?? [];
  if (to.length) return to.map((a) => a.address).join(', ');
  return e.envelope.rcptTo.join(', ');
}

/** How a message reached FastMock, as a short label: `SMTP`, `inject`, or the provider (`SES`, `SendGrid`, ...). */
const PROVIDERS: Record<string, string> = {
  ses: 'SES',
  sendgrid: 'SendGrid',
  mailgun: 'Mailgun',
  postmark: 'Postmark',
  resend: 'Resend',
  brevo: 'Brevo',
};

export function providerLabel(p: string | null | undefined): string {
  if (!p) return 'provider';
  return PROVIDERS[p.toLowerCase()] ?? p;
}

export function sourceLabel(e: Pick<MailEntry, 'source' | 'provider'>): string {
  if (e.source === 'provider') return providerLabel(e.provider);
  if (e.source === 'smtp') return 'SMTP';
  return 'inject';
}

/* ---------- unread markers (per browser) ---------- */

const SEEN_KEY = 'fastmock.mail.seen';
const SEEN_BEFORE_KEY = 'fastmock.mail.seenBefore';
const SEEN_MAX = 2000;

export interface SeenState {
  ids: Set<string>;
  /** Messages received at or before this time count as read. */
  before: number;
}

export function loadSeen(): SeenState {
  let ids: string[] = [];
  try {
    const v = JSON.parse(readLocal(SEEN_KEY) ?? '[]') as unknown;
    if (Array.isArray(v)) ids = v.filter((x): x is string => typeof x === 'string');
  } catch {
    ids = [];
  }
  const before = Number(readLocal(SEEN_BEFORE_KEY) ?? '0');
  return { ids: new Set(ids), before: Number.isFinite(before) ? before : 0 };
}

export function isUnread(seen: SeenState, e: Pick<MailEntry, 'id' | 'receivedAt'>): boolean {
  if (seen.ids.has(e.id)) return false;
  const at = Date.parse(e.receivedAt);
  return !(Number.isFinite(at) && at <= seen.before);
}

export function markSeen(seen: SeenState, id: string): SeenState {
  if (seen.ids.has(id)) return seen;
  const ids = [...seen.ids, id].slice(-SEEN_MAX);
  writeLocal(SEEN_KEY, JSON.stringify(ids));
  return { ...seen, ids: new Set(ids) };
}

export function markAllSeen(now = Date.now()): SeenState {
  writeLocal(SEEN_BEFORE_KEY, String(now));
  writeLocal(SEEN_KEY, '[]');
  return { ids: new Set(), before: now };
}

/* ---------- search syntax ---------- */

export const QUERY_HELP: { term: string; meaning: string }[] = [
  { term: 'from:billing', meaning: 'sender address or name contains the value' },
  { term: 'to:alice@example.test', meaning: 'a To recipient or envelope recipient' },
  { term: 'cc:team', meaning: 'a Cc recipient' },
  { term: 'subject:"monthly invoice"', meaning: 'subject contains the value; quote values with spaces' },
  { term: 'has:attachment', meaning: 'messages with attachments' },
  { term: 'label:otp', meaning: 'messages with this label' },
  { term: 'after:2026-11-01', meaning: 'received after a date (YYYY-MM-DD or RFC 3339)' },
  { term: 'before:2026-11-30', meaning: 'received before a date' },
  { term: 'reset password', meaning: 'free text in the subject and the text body' },
];

/** The query with one more term (quoted when it has spaces), unless the query already holds it. */
export function addTerm(query: string, key: string, value: string): string {
  const v = /\s/.test(value) ? `"${value.replace(/"/g, '')}"` : value;
  const term = `${key}:${v}`;
  const q = query.trim();
  if (q.split(/\s+/).includes(term)) return q;
  return q ? `${q} ${term}` : term;
}

/* ---------- server-sent events ---------- */

export interface SseMessage {
  event: string;
  data: string;
  /** A comment line (`: ready`, `: heartbeat`); `event` and `data` are empty then. */
  comment?: string;
}

/**
 * An incremental parser for `text/event-stream`: feed it decoded chunks, it returns the complete events and
 * comments in order and keeps an unfinished event for the next chunk.
 */
export class SseParser {
  private buf = '';
  private event = '';
  private data: string[] = [];

  push(chunk: string): SseMessage[] {
    this.buf += chunk;
    const out: SseMessage[] = [];
    for (;;) {
      const m = /\r\n|\r|\n/.exec(this.buf);
      if (!m) break;
      if (m[0] === '\r' && m.index === this.buf.length - 1) break;
      const line = this.buf.slice(0, m.index);
      this.buf = this.buf.slice(m.index + m[0].length);
      if (line === '') {
        if (this.data.length || this.event) out.push({ event: this.event || 'message', data: this.data.join('\n') });
        this.event = '';
        this.data = [];
        continue;
      }
      if (line.startsWith(':')) {
        out.push({ event: '', data: '', comment: line.slice(1).trim() });
        continue;
      }
      const colon = line.indexOf(':');
      const field = colon < 0 ? line : line.slice(0, colon);
      let value = colon < 0 ? '' : line.slice(colon + 1);
      if (value.startsWith(' ')) value = value.slice(1);
      if (field === 'event') this.event = value;
      else if (field === 'data') this.data.push(value);
    }
    return out;
  }
}

/* ---------- HTML body for a sandboxed srcdoc frame ---------- */

export interface PreparedHtml {
  html: string;
  /** Remote images removed (blocked mode) or left in place (allowed mode). */
  remoteImages: number;
}

const BLOCKED_TAGS = ['script', 'noscript', 'iframe', 'frame', 'frameset', 'object', 'embed', 'applet', 'base', 'link', 'form', 'portal'];

function isLocalUrl(u: string): boolean {
  return /^\s*(data:|cid:|#)/i.test(u) || u.trim() === '';
}

/** CSS declarations of a style attribute, each made `!important` so they win over the message's style sheets. */
function importantDecls(style: string): string {
  return style
    .replace(/[{}<>]/g, ' ')
    .split(';')
    .map((d) => d.trim())
    .filter((d) => d.includes(':'))
    .map((d) => (/!\s*important\s*$/i.test(d) ? d : `${d} !important`))
    .join('; ');
}

function stripRemoteUrls(css: string): { css: string; count: number } {
  let count = 0;
  const out = css.replace(/url\(\s*(['"]?)([^'")]*)\1\s*\)/gi, (all, _q: string, u: string) => {
    if (isLocalUrl(u)) return all;
    count += 1;
    return 'none';
  });
  return { css: out.replace(/@import[^;]*;?/gi, ''), count };
}

const BASE_CSS = ':root{color-scheme:light;background:#fff;color:#1a1522}body{margin:0;padding:16px;font-family:system-ui,sans-serif;overflow-wrap:anywhere}img{max-width:100%;height:auto}';

/**
 * Takes the inline CSS out of a message before it is parsed: the `<style>` elements (returned in order) and the
 * `style` attributes (renamed `data-fm-style`). Parsing them, even into an inert document, would make the browser
 * apply and refuse them under the UI's Content Security Policy.
 */
function liftInlineCss(source: string): { html: string; sheets: string[] } {
  const sheets: string[] = [];
  const withoutSheets = source.replace(/<style\b[^>]*>([\s\S]*?)(<\/style\s*>|$)/gi, (_all, css: string) => {
    sheets.push(css);
    return '';
  });
  const html = withoutSheets.replace(/<[a-zA-Z][^>]*>/g, (tag) => tag.replace(/(\s)style(\s*=)/gi, '$1data-fm-style$2'));
  return { html, sheets };
}

function styleTag(css: string, nonce?: string): string {
  const n = nonce ? ` nonce="${nonce.replace(/[^A-Za-z0-9+/=_-]/g, '')}"` : '';
  return `<style${n}>${css.replace(/<\/?style/gi, '')}</style>`;
}

/**
 * Prepares an HTML body for `<iframe sandbox srcdoc>`. A srcdoc document inherits the UI's Content Security
 * Policy, which allows only nonce-carrying style elements, so the message's `<style>` elements get the page's
 * style nonce and its `style` attributes become `!important` rules of one nonce-carrying sheet. Scripts, frames,
 * forms, base and link elements, event handler attributes and `javascript:` URLs are removed (the sandbox would
 * block them anyway). Remote images and CSS URLs are removed unless `allowImages`.
 */
export function prepareMailHtml(source: string, opts: { nonce?: string; allowImages?: boolean } = {}): PreparedHtml {
  const lifted = liftInlineCss(source);
  const doc = new DOMParser().parseFromString(lifted.html, 'text/html');
  for (const t of BLOCKED_TAGS) doc.querySelectorAll(t).forEach((el) => el.remove());
  doc.querySelectorAll('meta').forEach((m) => {
    if (m.getAttribute('http-equiv')) m.remove();
  });
  let remote = 0;
  const strip = (css: string): string => {
    if (opts.allowImages) return css;
    const s = stripRemoteUrls(css);
    remote += s.count;
    return s.css;
  };
  const rules: string[] = [];
  let n = 0;
  doc.querySelectorAll('*').forEach((el) => {
    for (const attr of [...el.attributes]) {
      const name = attr.name.toLowerCase();
      if (name.startsWith('on') || name === 'srcset' || name === 'nonce' || name === 'style') {
        el.removeAttribute(attr.name);
        continue;
      }
      if ((name === 'href' || name === 'src' || name === 'action' || name === 'formaction' || name === 'xlink:href') && /^\s*(javascript|vbscript):/i.test(attr.value)) {
        el.removeAttribute(attr.name);
      }
    }
    if (el.tagName === 'IMG' || el.tagName === 'INPUT') {
      const src = el.getAttribute('src');
      if (src !== null && !isLocalUrl(src)) {
        remote += 1;
        if (!opts.allowImages) {
          el.setAttribute('data-fm-blocked-src', src);
          el.removeAttribute('src');
        }
      }
    }
    const bg = el.getAttribute('background');
    if (bg !== null && !isLocalUrl(bg)) {
      remote += 1;
      if (!opts.allowImages) el.removeAttribute('background');
    }
    const style = el.getAttribute('data-fm-style');
    if (style !== null) {
      el.removeAttribute('data-fm-style');
      const decls = importantDecls(strip(style));
      if (decls) {
        n += 1;
        el.setAttribute('data-fm-s', String(n));
        rules.push(`[data-fm-s="${String(n)}"][data-fm-s="${String(n)}"]{${decls}}`);
      }
    }
  });
  const head = doc.head;
  const meta = doc.createElement('meta');
  meta.setAttribute('name', 'referrer');
  meta.setAttribute('content', 'no-referrer');
  head.insertBefore(meta, head.firstChild);
  const sheets = [BASE_CSS, ...lifted.sheets.map(strip), ...(rules.length ? [rules.join('\n')] : [])];
  const css = sheets.map((c) => styleTag(c, opts.nonce)).join('');
  const html = doc.documentElement.outerHTML.replace(/<head[^>]*>/i, (h) => `${h}${css}`);
  return { html: `<!doctype html>${html}`, remoteImages: remote };
}

/* ---------- inject form ---------- */

const ADDRESS = /^[^\s@<>()",;:]+@[^\s@<>()",;:]+$/;

/** `addr@example.com` or `Name <addr@example.com>`. */
export function validAddress(s: string): boolean {
  const t = s.trim();
  const m = /^(.*)<([^<>]+)>$/.exec(t);
  if (m) return ADDRESS.test((m[2] ?? '').trim()) && !/[\r\n]/.test(m[1] ?? '');
  return ADDRESS.test(t);
}

export interface ComposeIssues {
  from?: string;
  to?: string;
  cc?: string;
  subject?: string;
  body?: string;
  size?: string;
}

/** Default `FASTMOCK_MAIL_MAX_MESSAGE_BYTES`. */
export const MAX_MESSAGE_BYTES = 10 * 1024 * 1024;

/** What keeps a compose form from being sent, mirroring the server's checks. */
export function composeIssues(c: ComposeRequest): ComposeIssues {
  const out: ComposeIssues = {};
  if (!c.from.trim()) out.from = 'From is required';
  else if (!validAddress(c.from)) out.from = 'Use addr@example.com or Name <addr@example.com>';
  if (!c.to.length) out.to = 'Add at least one recipient';
  else {
    const bad = c.to.find((a) => !validAddress(a));
    if (bad) out.to = `Not an address: ${bad}`;
  }
  const badCc = (c.cc ?? []).find((a) => !validAddress(a));
  if (badCc) out.cc = `Not an address: ${badCc}`;
  if (c.subject && /[\r\n]/.test(c.subject)) out.subject = 'The subject must be one line';
  if (!c.text?.trim() && !c.html?.trim() && !(c.attachments ?? []).length) out.body = 'Write a text or HTML body, or attach a file';
  const size = (c.attachments ?? []).reduce((n, a) => n + Math.floor((a.base64.length * 3) / 4), 0) + (c.text?.length ?? 0) + (c.html?.length ?? 0);
  if (size > MAX_MESSAGE_BYTES) out.size = `The message is larger than ${String(MAX_MESSAGE_BYTES / 1024 / 1024)} MiB`;
  return out;
}

/** What keeps a raw message from being sent: it needs a header block before its body. */
export function rawIssue(raw: string): string | undefined {
  if (!raw.trim()) return 'Paste a message or choose an .eml file';
  const head = raw.split(/\r?\n\r?\n/)[0] ?? '';
  const first = head.split(/\r?\n/)[0] ?? '';
  if (!/^[!-9;-~]+:/.test(first)) return 'The message must start with header fields (Name: value)';
  if (raw.length > MAX_MESSAGE_BYTES) return `The message is larger than ${String(MAX_MESSAGE_BYTES / 1024 / 1024)} MiB`;
  return undefined;
}

export function fileToBase64(file: File): Promise<string> {
  return file.arrayBuffer().then((buf) => {
    const bytes = new Uint8Array(buf);
    let bin = '';
    const step = 0x8000;
    for (let i = 0; i < bytes.length; i += step) bin += String.fromCharCode(...bytes.subarray(i, i + step));
    return btoa(bin);
  });
}

/* ---------- chaos rules ---------- */

export const STAGES: { value: ChaosStage; label: string; hint: string }[] = [
  { value: 'connect', label: 'connect', hint: 'a new connection, before the greeting' },
  { value: 'tls', label: 'tls', hint: 'STARTTLS, or the SMTPS handshake' },
  { value: 'ehlo', label: 'ehlo', hint: 'EHLO / HELO' },
  { value: 'auth', label: 'auth', hint: 'AUTH, after the credentials' },
  { value: 'mail', label: 'mail', hint: 'MAIL FROM' },
  { value: 'rcpt', label: 'rcpt', hint: 'each RCPT TO' },
  { value: 'data', label: 'data', hint: 'end of the message, before it is stored' },
  { value: 'provider', label: 'provider', hint: 'a provider HTTP API call' },
];

export type MatchField = 'rcptTo' | 'mailFrom' | 'authUser' | 'provider';

export const MATCH_FIELDS: { field: MatchField; label: string }[] = [
  { field: 'rcptTo', label: 'Recipient' },
  { field: 'mailFrom', label: 'Sender' },
  { field: 'authUser', label: 'AUTH user' },
  { field: 'provider', label: 'Provider' },
];

/** The match fields a stage knows, as the server checks them. */
export function stageAllows(stage: ChaosStage, field: MatchField): boolean {
  switch (field) {
    case 'authUser':
      return ['auth', 'mail', 'rcpt', 'data'].includes(stage);
    case 'mailFrom':
      return ['mail', 'rcpt', 'data', 'provider'].includes(stage);
    case 'rcptTo':
      return ['rcpt', 'data', 'provider'].includes(stage);
    case 'provider':
      return stage === 'provider';
  }
}

const ID_RE = /^[A-Za-z0-9._-]{1,64}$/;

export function parseDelayMs(s: string): number | null {
  const m = /^(\d+)(ms|s|m)?$/.exec(s.trim());
  if (!m) return null;
  const n = Number(m[1]);
  const unit = m[2] ?? 's';
  return unit === 'ms' ? n : unit === 's' ? n * 1000 : n * 60_000;
}

export type ChaosIssues = Partial<Record<'id' | 'stage' | 'match' | 'action' | 'code' | 'enhanced' | 'text' | 'delay' | 'providerError' | 'times' | 'every' | 'probability' | 'seed' | 'limit', string>>;

/** What the server would refuse in a rule (and in its place among `others`), with the field it concerns. */
export function mailChaosIssues(r: MailChaosRule, others: readonly MailChaosRule[], max = 100): ChaosIssues {
  const out: ChaosIssues = {};
  if (others.length >= max) out.limit = `At most ${String(max)} mail chaos rules per space`;
  const id = r.id?.trim();
  if (id) {
    if (!ID_RE.test(id)) out.id = '1-64 letters, digits, ., _ or -';
    else if (others.some((o) => o.id === id)) out.id = `Id ${id} is used twice`;
  }
  for (const { field } of MATCH_FIELDS) {
    const m = r.match?.[field];
    if (m && !stageAllows(r.stage, field)) out.match = `${field} does not apply at stage ${r.stage}`;
    if (m?.matches) {
      try {
        new RegExp(m.matches);
      } catch {
        out.match = `${field}: invalid regular expression`;
      }
    }
  }
  const a = r.action;
  if (a.delay != null && a.delay !== '') {
    const ms = parseDelayMs(a.delay);
    if (ms === null) out.delay = 'Use 500ms, 2s or 1m';
    else if (ms > 600_000) out.delay = 'At most 10 minutes';
  }
  const effects = [!!a.reply, !!a.disconnect, !!a.providerError].filter(Boolean).length;
  if (effects > 1) out.action = 'Choose one of reply, disconnect and provider error';
  else if (effects === 0 && !a.delay) out.action = 'Choose an effect: reply, delay, disconnect or provider error';
  if (r.stage === 'provider') {
    if (a.reply || a.disconnect) out.action = 'Stage provider takes a provider error or a delay';
  } else if (a.providerError) out.action = 'A provider error applies only at stage provider';
  if (a.providerError && !ID_RE.test(a.providerError)) out.providerError = 'A name of letters, digits, ., _ or -';
  if (a.reply) {
    const c = a.reply.code;
    if (!Number.isInteger(c) || c < 400 || c > 599) out.code = 'A 4xx or 5xx code';
    const e = a.reply.enhanced;
    if (e) {
      if (!/^\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(e)) out.enhanced = 'Looks like 4.3.0';
      else if (e.split('.')[0] !== String(Math.floor(c / 100))) out.enhanced = `Must start with ${String(Math.floor(c / 100))} like the code`;
    }
    const t = a.reply.text;
    // eslint-disable-next-line no-control-regex
    if (t && (t.length > 200 || /[\u0000-\u001f\u007f]/.test(t))) out.text = 'At most 200 characters on one line';
  }
  if (r.times != null && r.times < 1) out.times = 'At least 1';
  if (r.every != null && r.every < 1) out.every = 'At least 1';
  if (r.probability != null && !(r.probability >= 0 && r.probability <= 100)) out.probability = 'Between 0 and 100';
  if (r.seed != null && r.probability == null) out.seed = 'A seed needs a probability';
  return out;
}

export type MatcherOp = 'contains' | 'equalTo' | 'matches';

/** A matcher the form can edit (`contains`, `equalTo` or `matches` with a value), or null for any other shape. */
export function simpleMatcher(m: MailStringMatcher | null | undefined): { op: MatcherOp; value: string; caseInsensitive: boolean } | null {
  if (!m) return null;
  const keys = Object.keys(m).filter((k) => (m as Record<string, unknown>)[k] != null && k !== 'caseInsensitive');
  if (keys.length !== 1) return null;
  const op = keys[0] as MatcherOp;
  if (!['contains', 'equalTo', 'matches'].includes(op)) return null;
  const v = m[op];
  return typeof v === 'string' ? { op, value: v, caseInsensitive: !!m.caseInsensitive } : null;
}

export function matcherText(m: MailStringMatcher | null | undefined): string {
  const s = simpleMatcher(m);
  if (!s) return m ? JSON.stringify(m) : '';
  const op = s.op === 'equalTo' ? '=' : s.op === 'contains' ? '∋' : '~';
  return `${op} ${s.value}${s.caseInsensitive ? ' (any case)' : ''}`;
}

/** A short description of what a rule does: `451 4.3.0 Try later`, `delay 2s`, `disconnect`, ... */
export function actionText(r: MailChaosRule): string[] {
  const a = r.action;
  const out: string[] = [];
  if (a.delay) out.push(`delay ${a.delay}`);
  if (a.reply) out.push([String(a.reply.code), a.reply.enhanced, a.reply.text].filter(Boolean).join(' '));
  if (a.disconnect) out.push('disconnect');
  if (a.providerError) out.push(`error ${a.providerError}`);
  return out;
}

/** `1st`, `2nd`, `3rd`, `4th`, `11th`, `22nd`, ... */
export function ordinal(n: number): string {
  const tens = n % 100;
  const suffix = tens >= 11 && tens <= 13 ? 'th' : (['th', 'st', 'nd', 'rd'][n % 10] ?? 'th');
  return `${String(n)}${suffix}`;
}

export function firingText(r: MailChaosRule): string {
  const parts: string[] = [];
  if (r.every) parts.push(r.every === 1 ? 'every time' : `every ${ordinal(r.every)}`);
  if (r.probability != null) parts.push(`${String(r.probability)}%${r.seed != null ? ` (seed ${String(r.seed)})` : ''}`);
  if (r.times) parts.push(`at most ${String(r.times)}×`);
  return parts.join(' · ') || 'always';
}

/** The rule without empty optional fields, as the server stores it. */
export function cleanRule(r: MailChaosRule): MailChaosRule {
  const out: MailChaosRule = { stage: r.stage, action: {} };
  const id = r.id?.trim();
  if (id) out.id = id;
  const match: NonNullable<MailChaosRule['match']> = {};
  for (const { field } of MATCH_FIELDS) {
    const m = r.match?.[field];
    if (m) match[field] = m;
  }
  if (Object.keys(match).length) out.match = match;
  const a = r.action;
  if (a.reply) {
    out.action.reply = { code: a.reply.code };
    if (a.reply.enhanced) out.action.reply.enhanced = a.reply.enhanced;
    if (a.reply.text) out.action.reply.text = a.reply.text;
  }
  if (a.delay) out.action.delay = a.delay.trim();
  if (a.disconnect) out.action.disconnect = true;
  if (a.providerError) out.action.providerError = a.providerError.trim();
  if (r.times != null) out.times = r.times;
  if (r.every != null) out.every = r.every;
  if (r.probability != null) out.probability = r.probability;
  if (r.seed != null) out.seed = r.seed;
  return out;
}

/* ---------- snippets ---------- */

export function smtpSnippets(host: string, port: number, tls = false): { id: string; label: string; code: string }[] {
  const server = `${host}:${String(port)}`;
  return [
    {
      id: 'swaks',
      label: 'swaks',
      code: `swaks --server ${server}${tls ? ' --tls-on-connect' : ''} \\\n  --from sender@example.com --to alice@example.test \\\n  --header 'Subject: Hello from swaks' --body 'It works.'`,
    },
    {
      id: 'curl',
      label: 'curl',
      code: `printf 'From: sender@example.com\\r\\nTo: alice@example.test\\r\\nSubject: Hello from curl\\r\\n\\r\\nIt works.\\r\\n' \\\n  | curl -s ${tls ? 'smtps' : 'smtp'}://${server} --mail-from sender@example.com \\\n      --mail-rcpt alice@example.test -T -`,
    },
    {
      id: 'python',
      label: 'Python',
      code: `import smtplib
from email.message import EmailMessage

msg = EmailMessage()
msg["From"] = "sender@example.com"
msg["To"] = "alice@example.test"
msg["Subject"] = "Hello from Python"
msg.set_content("It works.")

with smtplib.${tls ? 'SMTP_SSL' : 'SMTP'}("${host}", ${String(port)}) as smtp:
    smtp.send_message(msg)`,
    },
  ];
}

/** Mail settings the server publishes in `/ui/config.json`; a port is missing when its listener is off. */
export interface MailServerInfo {
  /** The server publishes these fields (older servers publish none). */
  published: boolean;
  /** `FASTMOCK_MAIL_HOSTNAME`, when it is set. */
  hostname?: string;
  smtp?: number;
  smtps?: number;
  imap?: number;
  imaps?: number;
  pop3?: number;
  pop3s?: number;
  /** IMAP, POP3 and SMTP AUTH accept a password before TLS; `false` means STARTTLS/STLS or a TLS port first. */
  plaintextAuth?: boolean;
}

const PORT_KEYS = { smtp: 'mailSmtpPort', smtps: 'mailSmtpsPort', imap: 'mailImapPort', imaps: 'mailImapsPort', pop3: 'mailPop3Port', pop3s: 'mailPop3sPort' } as const;

export function mailServerInfo(cfg: Record<string, unknown>): MailServerInfo {
  const out: MailServerInfo = { published: Object.values(PORT_KEYS).some((k) => k in cfg) };
  for (const [name, key] of Object.entries(PORT_KEYS) as [keyof typeof PORT_KEYS, string][]) {
    const v = cfg[key];
    if (typeof v === 'number') out[name] = v;
  }
  if (typeof cfg.mailHostname === 'string' && cfg.mailHostname) out.hostname = cfg.mailHostname;
  if (typeof cfg.mailPlaintextAuth === 'boolean') out.plaintextAuth = cfg.mailPlaintextAuth;
  return out;
}

export interface MailEndpoint {
  label: string;
  port?: number;
  tls: boolean;
}

/** The listeners of a protocol that are on: the plain one, then the one with implicit TLS. */
export function mailEndpoints(info: MailServerInfo, proto: 'smtp' | 'imap' | 'pop3'): MailEndpoint[] {
  const name = proto.toUpperCase();
  const plain = info[proto];
  const tls = info[`${proto}s`];
  return [...(plain !== undefined ? [{ label: name, port: plain, tls: false }] : []), ...(tls !== undefined ? [{ label: `${name}S`, port: tls, tls: true }] : [])];
}

/** `IMAPS :1993`, or the bare label when the port is not known. */
export function endpointText(e: MailEndpoint): string {
  return e.port !== undefined ? `${e.label} :${String(e.port)}` : e.label;
}

/** One sentence on whether IMAP and POP3 logins need TLS first, or `null` when the server does not say. */
export function loginRule(info: MailServerInfo, imap: boolean, pop3: boolean): string | null {
  if (info.plaintextAuth === undefined || (!imap && !pop3)) return null;
  if (info.plaintextAuth) return 'Plain-text logins are accepted: a client may send the password without TLS.';
  const imapEps = imap ? mailEndpoints(info, 'imap') : [];
  const pop3Eps = pop3 ? mailEndpoints(info, 'pop3') : [];
  const eps = [...imapEps, ...pop3Eps];
  const plain = eps.filter((e) => !e.tls).map(endpointText);
  const tls = eps.filter((e) => e.tls).map(endpointText);
  if (!plain.length) return `Logins only over TLS${tls.length ? `: ${tls.join(', ')}` : ''}.`;
  const upgrade = [imapEps.some((e) => !e.tls) ? 'STARTTLS (IMAP)' : '', pop3Eps.some((e) => !e.tls) ? 'STLS (POP3)' : ''].filter(Boolean).join(' or ');
  return `Plain-text logins are off: on ${plain.join(', ')} the client must start TLS with ${upgrade} before logging in${tls.length ? `, or connect to ${tls.join(', ')}` : ''}.`;
}

/** The address in `Name <addr>` or a bare address, lowercase; `undefined` without one. */
export function bareAddress(s: string | undefined): string | undefined {
  const m = /[^\s<>(),;:"]+@[^\s<>(),;:"]+/.exec(s ?? '');
  return m ? m[0].toLowerCase() : undefined;
}

/** The domain of an address, lowercase and without a trailing dot. */
export function domainOf(address: string): string | undefined {
  const at = address.lastIndexOf('@');
  const d = at < 0 ? '' : address.slice(at + 1).replace(/\.+$/, '').toLowerCase();
  return d || undefined;
}

/** Whether a `mailDomains` entry (`billing.example.test` or `*.billing.example.test`) covers a domain. */
function domainCovers(pattern: string, domain: string): boolean {
  const p = pattern.trim().replace(/\.+$/, '').toLowerCase();
  if (!p.startsWith('*.')) return p === domain;
  const base = p.slice(1);
  return domain.length > base.length && domain.endsWith(base);
}

/** The space whose `mailDomains` own a domain, as the server routes mail and resolves logins; `null` for none. */
export function mailDomainOwner(domain: string, spaces: readonly { name: string; mailDomains?: readonly string[] }[]): string | null {
  return spaces.find((s) => (s.mailDomains ?? []).some((p) => domainCovers(p, domain)))?.name ?? null;
}

/** The first recipient of a raw message: from the To, Cc and Bcc header fields. */
export function rawRecipient(raw: string): string | undefined {
  const head = raw.replace(/\r\n/g, '\n').split(/\n\n/, 1)[0] ?? '';
  const fields = head.replace(/\n[ \t]+/g, ' ').split('\n');
  for (const name of ['to', 'cc', 'bcc']) {
    const f = fields.find((l) => l.toLowerCase().startsWith(`${name}:`));
    const a = bareAddress(f?.slice(name.length + 1));
    if (a) return a;
  }
  return undefined;
}

/**
 * A curl command that waits for a message like this one (same sender, first recipient and subject) through
 * `POST /__admin/mail/messages/wait`, for a test to copy.
 */
export function waitSnippet(e: MailEntry, adminOrigin: string): string {
  const match: Record<string, MailStringMatcher> = {};
  const from = e.summary.from?.[0]?.address || e.envelope.mailFrom;
  if (from) match.from = { contains: from };
  const to = e.summary.to?.[0]?.address || e.envelope.rcptTo[0];
  if (to) match.to = { contains: to };
  if (e.summary.subject) match.subject = { equalTo: e.summary.subject };
  const body = JSON.stringify({ match, count: 1, timeoutMs: 10000, includeExisting: true });
  const q = e.space ? `?space=${encodeURIComponent(e.space)}` : '';
  return [
    `curl -sf -X POST ${shellQuote(`${adminOrigin.replace(/\/+$/, '')}/__admin/mail/messages/wait${q}`)}`,
    `  -H "Authorization: Bearer $FASTMOCK_TOKEN"`,
    `  -H 'Content-Type: application/json'`,
    `  -d ${shellQuote(body)}`,
  ].join(' \\\n');
}

/** A random password for a mailbox: 20 characters from a URL-safe alphabet. */
export function randomPassword(len = 20): string {
  const abc = 'ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789';
  const bytes = new Uint8Array(len);
  crypto.getRandomValues(bytes);
  return [...bytes].map((b) => abc[b % abc.length]).join('');
}
