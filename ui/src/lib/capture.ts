import type { CaptureSource, DecodedBody, MessageMatch, MessageProtocol, MessageSummary, SourceView } from '../api/capture';
import type { Tone } from './ui';

export const PROTOCOLS: { value: MessageProtocol; label: string }[] = [
  { value: 'kafka', label: 'Kafka' },
  { value: 'mqtt', label: 'MQTT' },
  { value: 'amqp', label: 'AMQP' },
];

export function protocolLabel(p: string | null | undefined): string {
  return PROTOCOLS.find((x) => x.value === p)?.label ?? (p || 'Kafka');
}

/** Where a message went: the topic, or for AMQP the exchange and routing key. */
export function destinationOf(m: Pick<MessageSummary, 'protocol' | 'topic' | 'exchange'>): string {
  if (m.protocol !== 'amqp') return m.topic;
  const ex = m.exchange || '(default)';
  return m.topic ? `${ex} → ${m.topic}` : ex;
}

/** What a source subscribes to, for lists. */
export function sourceTargets(s: Pick<CaptureSource, 'protocol' | 'topics' | 'exchange' | 'bindingKeys'>): string {
  if (s.protocol === 'amqp') {
    const keys = s.bindingKeys?.length ? s.bindingKeys : ['#'];
    return `${s.exchange ?? ''} [${keys.join(', ')}]`;
  }
  return (s.topics ?? []).join(', ');
}

export function stateTone(state: string): Tone {
  switch (state) {
    case 'running':
      return 'success';
    case 'connecting':
      return 'info';
    case 'error':
      return 'danger';
    case 'stopped':
      return 'warning';
    default:
      return 'neutral';
  }
}

/** Time ranges the message list offers, in minutes (0: any time). */
export const RANGES: { value: string; label: string; minutes: number }[] = [
  { value: 'any', label: 'Any time', minutes: 0 },
  { value: '5m', label: 'Last 5 minutes', minutes: 5 },
  { value: '15m', label: 'Last 15 minutes', minutes: 15 },
  { value: '1h', label: 'Last hour', minutes: 60 },
  { value: '24h', label: 'Last 24 hours', minutes: 1440 },
  { value: '7d', label: 'Last 7 days', minutes: 10080 },
];

/** The `since` of a range relative to `now`, or undefined for any time. */
export function sinceOf(range: string | null | undefined, now: number = Date.now()): string | undefined {
  const r = RANGES.find((x) => x.value === range);
  if (!r?.minutes) return undefined;
  return new Date(now - r.minutes * 60_000).toISOString();
}

/** A JSON match for `POST /capture/messages/find`: null when empty, an error text when invalid. */
export function parseMatch(text: string): { match: MessageMatch | null; error?: string } {
  if (!text.trim()) return { match: null };
  try {
    const v = JSON.parse(text) as unknown;
    if (!v || typeof v !== 'object' || Array.isArray(v)) return { match: null, error: 'A match is a JSON object' };
    return { match: v };
  } catch (e) {
    return { match: null, error: e instanceof Error ? e.message : String(e) };
  }
}

/** The match with the list filters added, unless the match sets the same field itself. */
export function withFilters(match: MessageMatch, f: { source?: string; protocol?: string; topic?: string }): MessageMatch {
  const out: MessageMatch = { ...match };
  if (f.source && out.source == null) out.source = { equalTo: f.source };
  if (f.protocol && out.protocol == null) out.protocol = f.protocol as MessageProtocol;
  if (f.topic && out.topic == null) out.topic = { equalTo: f.topic };
  return out;
}

export type BodyRendering =
  | { kind: 'json'; text: string; schemaId?: number | null }
  | { kind: 'text'; text: string }
  | { kind: 'base64'; text: string; error?: string | null }
  | { kind: 'empty' };

/** How to show a decoded body: JSON and Avro pretty-printed, text as is, other bytes as base64. */
export function bodyRendering(body: DecodedBody | null | undefined): BodyRendering {
  if (!body || body.encoding === 'empty') return { kind: 'empty' };
  const value = body.value as unknown;
  if (body.encoding === 'json' || body.encoding === 'avro') return { kind: 'json', text: JSON.stringify(value, null, 2), schemaId: body.encoding === 'avro' ? (body.schemaId ?? null) : undefined };
  const text = typeof value === 'string' ? value : JSON.stringify(value);
  if (body.encoding === 'text') return { kind: 'text', text };
  return { kind: 'base64', text, error: body.error };
}

export interface SourceDraft {
  id?: string;
  name: string;
  protocol: MessageProtocol;
  topics: string[];
  startFrom: 'latest' | 'earliest';
  qos: number;
  exchange: string;
  bindingKeys: string[];
  maxMessages: number | '';
  enabled: boolean;
  description: string;
}

export const EMPTY_SOURCE: SourceDraft = {
  name: '',
  protocol: 'kafka',
  topics: [],
  startFrom: 'latest',
  qos: 1,
  exchange: '',
  bindingKeys: [],
  maxMessages: '',
  enabled: true,
  description: '',
};

export function sourceToDraft(s: SourceView | CaptureSource): SourceDraft {
  return {
    ...(s.id ? { id: s.id } : {}),
    name: s.name,
    protocol: s.protocol,
    topics: s.topics ?? [],
    startFrom: s.startFrom === 'earliest' ? 'earliest' : 'latest',
    qos: s.qos ?? 1,
    exchange: s.exchange ?? '',
    bindingKeys: s.bindingKeys ?? [],
    maxMessages: s.maxMessages ?? '',
    enabled: s.enabled ?? true,
    description: s.description ?? '',
  };
}

/** The `PUT /capture/sources/{id}` body: only the fields of the chosen protocol. */
export function draftToSource(d: SourceDraft): CaptureSource {
  const out: CaptureSource = { name: d.name.trim(), protocol: d.protocol, enabled: d.enabled };
  const topics = d.topics.map((t) => t.trim()).filter(Boolean);
  if (d.protocol === 'kafka') {
    out.topics = topics;
    out.startFrom = d.startFrom;
  } else if (d.protocol === 'mqtt') {
    out.topics = topics;
    out.qos = d.qos;
  } else {
    out.exchange = d.exchange.trim();
    const keys = d.bindingKeys.map((k) => k.trim()).filter(Boolean);
    if (keys.length) out.bindingKeys = keys;
  }
  if (d.maxMessages !== '') out.maxMessages = d.maxMessages;
  if (d.description.trim()) out.description = d.description.trim();
  return out;
}

const MAX_TOPICS = 20;
const NAME_RE = /^[a-z0-9._-]{1,64}$/;
const KAFKA_TOPIC_RE = /^[A-Za-z0-9._-]{1,249}$/;
const EXCHANGE_RE = /^[A-Za-z0-9_.:/-]{1,255}$/;

export function validMqttFilter(f: string): boolean {
  if (!f || f.length > 65535) return false;
  const levels = f.split('/');
  return levels.every((l, i) => (l.includes('+') || l.includes('#') ? l === '+' || (l === '#' && i === levels.length - 1) : true));
}

export type SourceIssues = Partial<Record<'name' | 'topics' | 'exchange' | 'bindingKeys' | 'maxMessages' | 'description', string>>;

/** The checks of the server, so the form can say what is wrong before saving. */
export function sourceIssues(d: SourceDraft, maxMessages?: number): SourceIssues {
  const out: SourceIssues = {};
  if (!NAME_RE.test(d.name.trim())) out.name = 'Use 1 to 64 characters of a-z 0-9 . _ -';
  const topics = d.topics.map((t) => t.trim()).filter(Boolean);
  if (d.protocol === 'kafka') {
    const bad = topics.find((t) => !KAFKA_TOPIC_RE.test(t) || t === '.' || t === '..');
    if (!topics.length) out.topics = 'Add at least one topic';
    else if (bad) out.topics = `${bad} is not a valid Kafka topic name`;
  } else if (d.protocol === 'mqtt') {
    const bad = topics.find((t) => !validMqttFilter(t));
    if (!topics.length) out.topics = 'Add at least one topic filter';
    else if (bad) out.topics = `${bad} is not a valid topic filter (+ and # only as whole levels, # last)`;
  } else {
    const ex = d.exchange.trim();
    if (!ex || ex === 'amq.default') out.exchange = 'Name an exchange; the default exchange cannot be bound';
    else if (!EXCHANGE_RE.test(ex)) out.exchange = 'Use letters, digits and - _ . : /';
    if (d.bindingKeys.some((k) => k.length > 255)) out.bindingKeys = 'Binding keys are at most 255 bytes';
    if (d.bindingKeys.length > MAX_TOPICS) out.bindingKeys = `At most ${String(MAX_TOPICS)} binding keys`;
  }
  if (topics.length > MAX_TOPICS && !out.topics) out.topics = `At most ${String(MAX_TOPICS)} topics`;
  if (d.maxMessages !== '' && d.maxMessages < 1) out.maxMessages = 'At least 1';
  else if (d.maxMessages !== '' && maxMessages && d.maxMessages > maxMessages) out.maxMessages = `At most ${String(maxMessages)} (the server limit)`;
  if (new TextEncoder().encode(d.description).length > 1024) out.description = 'At most 1024 bytes';
  return out;
}

/** A random UUID v4; `crypto.randomUUID` exists only in secure contexts, so plain-HTTP pages build one. */
export function newSourceId(): string {
  if (typeof crypto.randomUUID === 'function') return crypto.randomUUID();
  const b = crypto.getRandomValues(new Uint8Array(16));
  b[6] = ((b[6] ?? 0) & 0x0f) | 0x40;
  b[8] = ((b[8] ?? 0) & 0x3f) | 0x80;
  const h = [...b].map((x) => x.toString(16).padStart(2, '0')).join('');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}
