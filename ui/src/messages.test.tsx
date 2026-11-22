import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { AppRoutes } from './App';
import type { MessageDetail, MessageSummary, SourceView } from './api/capture';
import { tokenStore } from './api/http';
import { BUILTIN_ROLES } from './auth/permissions';
import { visibleNav } from './components/nav';
import {
  bodyRendering,
  destinationOf,
  draftToSource,
  EMPTY_SOURCE,
  newSourceId,
  parseMatch,
  sinceOf,
  sourceIssues,
  sourceTargets,
  sourceToDraft,
  validMqttFilter,
  withFilters,
} from './lib/capture';
import { renderApp } from './test/render';

const ADMIN = { name: 'ops', role: 'admin', kind: 'token', permissions: { global: ['*'], spaces: {} } };
const VIEWER = { name: 'dash', role: 'viewer', kind: 'token', permissions: { global: ['stubs:read', 'messages:read', 'info:read'], spaces: {} } };

const SRC_ID = '01a1fb9c-0000-7000-8000-00000000a001';

const props = (p: Record<string, unknown>) => p as MessageSummary['properties'];

function summary(id: string, extra: Partial<MessageSummary> = {}): MessageSummary {
  return {
    id,
    sourceId: SRC_ID,
    source: 'orders',
    protocol: 'kafka',
    topic: 'orders.created',
    key: 'order-1',
    headers: { traceparent: '00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01' },
    properties: props({ partition: 0, offset: 42 }),
    contentType: 'application/json',
    size: 27,
    preview: '{"id":1,"status":"PAID"}',
    receivedAt: new Date(Date.now() - 30_000).toISOString(),
    ...extra,
  };
}

const M1 = summary('01a1fb9c-0000-7000-8000-000000000001');
const M2 = summary('01a1fb9c-0000-7000-8000-000000000002', { source: 'billing', protocol: 'amqp', topic: 'invoice.paid', exchange: 'billing', key: null, properties: props({ messageId: 'm-1' }) });
const M3 = summary('01a1fb9c-0000-7000-8000-000000000003', { source: 'devices', protocol: 'mqtt', topic: 'devices/7/telemetry', key: null, properties: props({ qos: 1, retain: false }) });

function status(extra: Partial<SourceView['status']> = {}): SourceView['status'] {
  return { state: 'running', received: 12, dropped: 0, stored: 10, connects: 1, ...extra };
}

const S_KAFKA: SourceView = { id: SRC_ID, name: 'orders', protocol: 'kafka', topics: ['orders.created'], startFrom: 'latest', enabled: true, status: status() };
const S_AMQP: SourceView = {
  id: '01a1fb9c-0000-7000-8000-00000000a002',
  name: 'billing',
  protocol: 'amqp',
  exchange: 'billing',
  bindingKeys: ['invoice.*'],
  enabled: true,
  fromConfig: true,
  status: status({ state: 'error', lastError: 'connection refused', stored: 0, received: 0 }),
};

interface Call {
  method: string;
  path: string;
  body: string | null;
}

type Reply = { status?: number; body?: unknown; text?: string; type?: string } | undefined;

function server(opts: { principal?: unknown; features?: string[]; route?: (method: string, url: URL, body: string | null) => Reply }) {
  const calls: Call[] = [];
  const fn = vi.fn(async (input: RequestInfo | URL, init?: RequestInit) => {
    const req = input instanceof Request ? input : new Request(new URL(String(input), 'http://localhost'), init);
    const url = new URL(req.url, 'http://localhost');
    const body = req.method === 'GET' || req.method === 'HEAD' ? null : await req.text();
    calls.push({ method: req.method, path: url.pathname + url.search, body });
    const json = (code: number, b: unknown) => new Response(code === 204 ? null : JSON.stringify(b), { status: code, headers: { 'content-type': 'application/json' } });
    if (url.pathname === '/__admin/info') return json(200, { name: 'FastMock', version: '0.1.0', storage: 'memory', uptimeSeconds: 1, stubsLoaded: 0, plugins: [], principal: opts.principal ?? ADMIN, authDisabled: false });
    if (url.pathname === '/__admin/version') return json(200, { version: '0.1.0', features: opts.features ?? ['capture', 'mqtt', 'amqp'] });
    const r = opts.route?.(req.method, url, body);
    if (!r) return json(404, { error: 'not found' });
    if (r.text !== undefined) return new Response(r.text, { status: r.status ?? 200, headers: { 'content-type': r.type ?? 'application/octet-stream' } });
    return json(r.status ?? 200, r.body ?? {});
  });
  vi.stubGlobal('fetch', fn);
  return calls;
}

function routes(pages: Record<string, { messages: MessageSummary[]; nextCursor?: string }>, sources: SourceView[] = [S_KAFKA, S_AMQP]) {
  return (method: string, url: URL): Reply => {
    if (url.pathname === '/__admin/capture/messages' && method === 'GET') return { body: pages[url.searchParams.get('cursor') ?? ''] ?? { messages: [] } };
    if (url.pathname === '/__admin/capture/sources' && method === 'GET') return { body: { sources, maxSources: 20, maxMessages: 1000 } };
    return undefined;
  };
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

describe('capture helpers', () => {
  it('builds source bodies with only the fields of the chosen protocol', () => {
    const base = { ...EMPTY_SOURCE, name: 'orders', topics: ['orders.created', ' '], exchange: 'left-over', bindingKeys: ['x'], qos: 2, maxMessages: 500 as const, description: ' Orders ' };
    expect(draftToSource({ ...base, protocol: 'kafka', startFrom: 'earliest' })).toEqual({ name: 'orders', protocol: 'kafka', enabled: true, topics: ['orders.created'], startFrom: 'earliest', maxMessages: 500, description: 'Orders' });
    expect(draftToSource({ ...base, protocol: 'mqtt' })).toEqual({ name: 'orders', protocol: 'mqtt', enabled: true, topics: ['orders.created'], qos: 2, maxMessages: 500, description: 'Orders' });
    expect(draftToSource({ ...base, protocol: 'amqp', exchange: 'orders', bindingKeys: [] })).toEqual({ name: 'orders', protocol: 'amqp', enabled: true, exchange: 'orders', maxMessages: 500, description: 'Orders' });
    expect(draftToSource({ ...base, protocol: 'amqp', exchange: 'orders', maxMessages: '', description: '' })).toEqual({ name: 'orders', protocol: 'amqp', enabled: true, exchange: 'orders', bindingKeys: ['x'] });
    expect(sourceToDraft(S_AMQP)).toMatchObject({ id: S_AMQP.id, protocol: 'amqp', exchange: 'billing', bindingKeys: ['invoice.*'], topics: [] });
  });

  it('checks sources like the server', () => {
    expect(sourceIssues({ ...EMPTY_SOURCE, name: 'Orders' })).toEqual({ name: 'Use 1 to 64 characters of a-z 0-9 . _ -', topics: 'Add at least one topic' });
    expect(sourceIssues({ ...EMPTY_SOURCE, name: 'a', topics: ['bad topic'] }).topics).toMatch(/not a valid Kafka topic/);
    expect(sourceIssues({ ...EMPTY_SOURCE, name: 'a', protocol: 'mqtt', topics: ['a/b#'] }).topics).toMatch(/topic filter/);
    expect(sourceIssues({ ...EMPTY_SOURCE, name: 'a', protocol: 'mqtt', topics: ['a/+/b', '#'] })).toEqual({});
    expect(sourceIssues({ ...EMPTY_SOURCE, name: 'a', protocol: 'amqp', exchange: 'amq.default' }).exchange).toMatch(/default exchange/);
    expect(sourceIssues({ ...EMPTY_SOURCE, name: 'a', topics: ['t'], maxMessages: 5000 }, 1000).maxMessages).toMatch(/At most 1000/);
    expect(validMqttFilter('sport/+/player1/#')).toBe(true);
    expect(validMqttFilter('sport/#/x')).toBe(false);
    expect(newSourceId()).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
  });

  it('names destinations, decodes bodies and builds matches', () => {
    expect(destinationOf(M1)).toBe('orders.created');
    expect(destinationOf(M2)).toBe('billing → invoice.paid');
    expect(sourceTargets(S_AMQP)).toBe('billing [invoice.*]');
    expect(sourceTargets({ protocol: 'amqp', exchange: 'x' })).toBe('x [#]');
    expect(bodyRendering({ encoding: 'json', value: { a: 1 } as never })).toEqual({ kind: 'json', text: '{\n  "a": 1\n}', schemaId: undefined });
    expect(bodyRendering({ encoding: 'avro', value: { a: 1 } as never, schemaId: 7 })).toMatchObject({ kind: 'json', schemaId: 7 });
    expect(bodyRendering({ encoding: 'text', value: 'hi' as never })).toEqual({ kind: 'text', text: 'hi' });
    expect(bodyRendering({ encoding: 'base64', value: 'AAE=' as never, error: 'unknown schema' })).toEqual({ kind: 'base64', text: 'AAE=', error: 'unknown schema' });
    expect(bodyRendering({ encoding: 'empty', value: null as never })).toEqual({ kind: 'empty' });
    expect(parseMatch('')).toEqual({ match: null });
    expect(parseMatch('[1]').error).toBe('A match is a JSON object');
    expect(parseMatch('{"key":{"equalTo":"k"}}').match).toEqual({ key: { equalTo: 'k' } });
    expect(withFilters({ topic: { contains: 'x' } }, { source: 'orders', protocol: 'kafka', topic: 'y' })).toEqual({ topic: { contains: 'x' }, source: { equalTo: 'orders' }, protocol: 'kafka' });
    expect(sinceOf('any')).toBeUndefined();
    expect(sinceOf('5m', Date.parse('2026-11-20T10:05:00Z'))).toBe('2026-11-20T10:00:00.000Z');
  });
});

describe('messages navigation and permissions', () => {
  it('shows Messages only with the capture feature and messages:read', () => {
    const labels = (perms: string[], f: string[]) => visibleNav({ global: perms, spaces: {} }, false, f).flatMap((g) => g.items.map((i) => i.label));
    expect(labels(['messages:read'], ['capture'])).toContain('Messages');
    expect(labels(['messages:read'], ['mqtt', 'amqp'])).not.toContain('Messages');
    expect(labels(['stubs:read'], ['capture'])).not.toContain('Messages');
    expect(BUILTIN_ROLES.viewer).toContain('messages:read');
    expect(BUILTIN_ROLES.viewer).not.toContain('messages:write');
    expect(BUILTIN_ROLES.editor).toEqual(expect.arrayContaining(['messages:read', 'messages:write', 'messages:delete']));
  });

  it('says how to enable capture when it is off', async () => {
    server({ features: ['mqtt', 'amqp'] });
    renderApp(<AppRoutes />, '/messages');
    expect(await screen.findByTestId('capture-off')).toHaveTextContent('FASTMOCK_CAPTURE_ENABLED=true');
  });
});

describe('message list', () => {
  it('lists messages newest first with protocol badges and pages with the cursor', async () => {
    const calls = server({ route: routes({ '': { messages: [M1, M2], nextCursor: M2.id }, [M2.id]: { messages: [M3] } }) });
    renderApp(<AppRoutes />, '/messages');
    const rows = await screen.findAllByTestId('message-row');
    expect(rows).toHaveLength(2);
    expect(rows[0]).toHaveTextContent('orders.created');
    expect(rows[0]).toHaveTextContent('order-1');
    expect(within(rows[0] as HTMLElement).getByTestId('protocol-badge')).toHaveTextContent('Kafka');
    expect(within(rows[1] as HTMLElement).getByTestId('message-destination')).toHaveTextContent('billing → invoice.paid');
    expect(within(rows[1] as HTMLElement).getByTestId('protocol-badge')).toHaveTextContent('AMQP');
    await userEvent.click(screen.getByTestId('messages-load-more'));
    await waitFor(() => expect(screen.getAllByTestId('message-row')).toHaveLength(3));
    expect(within(screen.getAllByTestId('message-row')[2] as HTMLElement).getByTestId('protocol-badge')).toHaveTextContent('MQTT');
    expect(calls.some((c) => c.path === `/__admin/capture/messages?cursor=${M2.id}&limit=50`)).toBe(true);
  });

  it('filters by source, protocol, topic and time range', async () => {
    const calls = server({ route: routes({ '': { messages: [M1] } }) });
    renderApp(<AppRoutes />, '/messages?source=orders&protocol=kafka&range=1h');
    await screen.findAllByTestId('message-row');
    await userEvent.type(screen.getByTestId('filter-topic'), 'orders.created{Enter}');
    await waitFor(() => {
      const last = calls.filter((c) => c.path.startsWith('/__admin/capture/messages?')).at(-1);
      const q = new URL(last?.path ?? '/', 'http://localhost').searchParams;
      expect(q.get('source')).toBe('orders');
      expect(q.get('protocol')).toBe('kafka');
      expect(q.get('topic')).toBe('orders.created');
      expect(Date.now() - Date.parse(q.get('since') ?? '')).toBeGreaterThan(59 * 60_000);
    });
  });

  it('finds messages with a JSON match plus the filters', async () => {
    const calls = server({
      route: (method, url) => {
        if (method === 'POST' && url.pathname === '/__admin/capture/messages/find') return { body: { messages: [M1], count: 1 } };
        return routes({ '': { messages: [M1, M2] } })(method, url);
      },
    });
    const match = JSON.stringify({ key: { equalTo: 'order-1' } });
    renderApp(<AppRoutes />, `/messages?source=orders&match=${encodeURIComponent(match)}`);
    expect(await screen.findAllByTestId('message-row')).toHaveLength(1);
    expect(screen.getByTestId('messages-count')).toHaveTextContent('1 matching message');
    const post = calls.find((c) => c.method === 'POST');
    expect(JSON.parse(post?.body ?? '{}')).toEqual({ match: { key: { equalTo: 'order-1' }, source: { equalTo: 'orders' } }, limit: 200 });
    await userEvent.click(screen.getByTestId('match-clear'));
    await waitFor(() => expect(screen.getAllByTestId('message-row')).toHaveLength(2));
  });

  it('deletes the messages of the selected source after confirmation', async () => {
    const calls = server({
      route: (method, url) => {
        if (method === 'DELETE' && url.pathname === '/__admin/capture/messages') return { body: { deleted: 2 } };
        return routes({ '': { messages: [M1] } })(method, url);
      },
    });
    renderApp(<AppRoutes />, '/messages?source=orders');
    await userEvent.click(await screen.findByTestId('messages-delete-all'));
    await userEvent.click(await screen.findByTestId('confirm-button'));
    await waitFor(() => expect(calls.find((c) => c.method === 'DELETE')?.path).toBe('/__admin/capture/messages?source=orders'));
  });

  it('gives a reader no delete controls', async () => {
    server({ principal: VIEWER, route: routes({ '': { messages: [M1] } }) });
    renderApp(<AppRoutes />, '/messages');
    await screen.findAllByTestId('message-row');
    expect(screen.queryByTestId('messages-delete-all')).not.toBeInTheDocument();
  });
});

describe('message view', () => {
  function detailServer(body: MessageDetail['body'], extra: Partial<MessageSummary> = {}) {
    const msg: MessageDetail = { ...M1, ...extra, body };
    const base = `/__admin/capture/messages/${msg.id}`;
    return server({
      route: (method, url) => {
        if (url.pathname === base && method === 'GET') return { body: msg };
        if (url.pathname === base && method === 'DELETE') return { status: 204 };
        if (url.pathname === `${base}/body`) return { text: 'raw-bytes' };
        return routes({ '': { messages: [] } })(method, url);
      },
    });
  }

  it('shows the metadata and the JSON body pretty-printed', async () => {
    detailServer({ encoding: 'json', value: { id: 1, status: 'PAID' } as never });
    renderApp(<AppRoutes />, `/messages/${M1.id}`);
    expect(await screen.findByRole('heading', { name: 'orders.created' })).toBeInTheDocument();
    const meta = screen.getByTestId('message-meta');
    expect(meta).toHaveTextContent('orders');
    expect(meta).toHaveTextContent('order-1');
    expect(meta).toHaveTextContent('application/json');
    expect(screen.getByTestId('message-headers')).toHaveTextContent('traceparent');
    expect(screen.getByTestId('message-properties')).toHaveTextContent('offset');
    expect(screen.getByTestId('body-encoding')).toHaveTextContent('json');
    await waitFor(() => expect(screen.getByTestId('body-json')).toHaveTextContent('"status": "PAID"'));
  });

  it('shows Avro bodies as JSON with the schema id and AMQP routing details', async () => {
    detailServer({ encoding: 'avro', value: { amount: 10 } as never, schemaId: 12 }, { protocol: 'amqp', exchange: 'billing', topic: 'invoice.paid', key: null });
    renderApp(<AppRoutes />, `/messages/${M1.id}`);
    expect(await screen.findByTestId('body-encoding')).toHaveTextContent('Avro · schema 12');
    const meta = screen.getByTestId('message-meta');
    expect(within(meta).getByText('Exchange')).toBeInTheDocument();
    expect(within(meta).getByText('Routing key')).toBeInTheDocument();
    expect(meta).toHaveTextContent('invoice.paid');
    await waitFor(() => expect(screen.getByTestId('body-json')).toHaveTextContent('"amount": 10'));
  });

  it('shows binary bodies as base64 and downloads the raw bytes', async () => {
    const created = vi.fn(() => 'blob:fastmock');
    Object.defineProperty(URL, 'createObjectURL', { value: created, configurable: true });
    Object.defineProperty(URL, 'revokeObjectURL', { value: () => undefined, configurable: true });
    const calls = detailServer({ encoding: 'base64', value: 'AAECAw==' as never, error: 'schema 9 is not registered' });
    renderApp(<AppRoutes />, `/messages/${M1.id}`);
    expect(await screen.findByTestId('body-base64')).toHaveTextContent('AAECAw==');
    expect(screen.getByTestId('body-error')).toHaveTextContent('schema 9 is not registered');
    await userEvent.click(screen.getByTestId('message-download'));
    await waitFor(() => expect(created).toHaveBeenCalled());
    expect(calls.some((c) => c.path === `/__admin/capture/messages/${M1.id}/body`)).toBe(true);
  });

  it('deletes the message after confirmation', async () => {
    const calls = detailServer({ encoding: 'text', value: 'hello' as never });
    renderApp(<AppRoutes />, `/messages/${M1.id}`);
    await userEvent.click(await screen.findByTestId('message-delete'));
    await userEvent.click(await screen.findByTestId('confirm-button'));
    await waitFor(() => expect(calls.some((c) => c.method === 'DELETE' && c.path === `/__admin/capture/messages/${M1.id}`)).toBe(true));
  });
});

describe('capture sources', () => {
  it('lists sources with their state, error and counters', async () => {
    server({ route: routes({}) });
    renderApp(<AppRoutes />, '/messages/sources');
    const rows = await screen.findAllByTestId('source-row');
    expect(rows).toHaveLength(2);
    expect(within(rows[0] as HTMLElement).getByTestId('source-state')).toHaveTextContent('running');
    expect(within(rows[0] as HTMLElement).getByTestId('source-stored')).toHaveTextContent('10 / 1000');
    expect(within(rows[1] as HTMLElement).getByTestId('source-state')).toHaveTextContent('error');
    expect(within(rows[1] as HTMLElement).getByTestId('source-error')).toHaveTextContent('connection refused');
    expect(within(rows[1] as HTMLElement).getByTestId('source-from-config')).toBeInTheDocument();
    expect(within(rows[1] as HTMLElement).queryByTestId('source-delete')).not.toBeInTheDocument();
    expect(screen.getByTestId('sources-limits')).toHaveTextContent('2 of 20 sources');
  });

  it('creates a source with the fields of the chosen protocol only', async () => {
    const calls = server({
      route: (method, url, body) => {
        if (method === 'PUT' && url.pathname.startsWith('/__admin/capture/sources/')) return { body: { ...(JSON.parse(body ?? '{}') as object), id: url.pathname.split('/').pop(), status: status({ state: 'connecting' }) } };
        return routes({})(method, url);
      },
    });
    renderApp(<AppRoutes />, '/messages/sources');
    await userEvent.click(await screen.findByTestId('source-add'));
    const dialog = await screen.findByRole('dialog', { name: 'Add a source' });
    await userEvent.click(within(dialog).getByTestId('source-save'));
    expect(await within(dialog).findByText('Add at least one topic')).toBeInTheDocument();
    await userEvent.type(within(dialog).getByTestId('source-name'), 'telemetry');
    await userEvent.type(within(dialog).getByTestId('source-topics'), 'left.over{Enter}');
    await userEvent.click(within(dialog).getByRole('radio', { name: 'MQTT' }));
    expect(within(dialog).queryByTestId('source-start-from')).not.toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('radio', { name: 'AMQP' }));
    expect(within(dialog).queryByTestId('source-topics')).not.toBeInTheDocument();
    await userEvent.type(within(dialog).getByTestId('source-exchange'), 'events');
    await userEvent.type(within(dialog).getByTestId('source-binding-keys'), 'device.#{Enter}');
    await userEvent.type(within(dialog).getByTestId('source-max-messages'), '250');
    await userEvent.click(within(dialog).getByTestId('source-save'));
    await waitFor(() => expect(calls.some((c) => c.method === 'PUT')).toBe(true));
    const put = calls.find((c) => c.method === 'PUT');
    expect(put?.path).toMatch(/^\/__admin\/capture\/sources\/[0-9a-f-]{36}$/);
    expect(JSON.parse(put?.body ?? '{}')).toEqual({ name: 'telemetry', protocol: 'amqp', enabled: true, exchange: 'events', bindingKeys: ['device.#'], maxMessages: 250 });
  });

  it('edits an MQTT source under its id', async () => {
    const mqtt: SourceView = { id: '01a1fb9c-0000-7000-8000-00000000a003', name: 'devices', protocol: 'mqtt', topics: ['devices/+/telemetry'], qos: 1, enabled: true, status: status() };
    const calls = server({
      route: (method, url, body) => {
        if (method === 'PUT') return { body: { ...(JSON.parse(body ?? '{}') as object), id: mqtt.id, status: status() } };
        return routes({}, [mqtt])(method, url);
      },
    });
    renderApp(<AppRoutes />, '/messages/sources');
    await userEvent.click(await screen.findByTestId('source-edit'));
    const dialog = await screen.findByRole('dialog', { name: 'Edit devices' });
    await userEvent.click(within(dialog).getByTestId('source-enabled'));
    await userEvent.click(within(dialog).getByTestId('source-save'));
    await waitFor(() => expect(calls.find((c) => c.method === 'PUT')?.path).toBe(`/__admin/capture/sources/${mqtt.id}`));
    expect(JSON.parse(calls.find((c) => c.method === 'PUT')?.body ?? '{}')).toEqual({ name: 'devices', protocol: 'mqtt', enabled: false, topics: ['devices/+/telemetry'], qos: 1 });
  });

  it('shows configured sources read-only and deletes others with their messages on request', async () => {
    const calls = server({
      route: (method, url) => {
        if (method === 'DELETE') return { status: 204 };
        return routes({})(method, url);
      },
    });
    renderApp(<AppRoutes />, '/messages/sources');
    const rows = await screen.findAllByTestId('source-row');
    await userEvent.click(within(rows[1] as HTMLElement).getByTestId('source-edit'));
    const dialog = await screen.findByRole('dialog', { name: 'billing' });
    expect(within(dialog).getByTestId('source-read-only')).toHaveTextContent('server configuration');
    expect(within(dialog).queryByTestId('source-save')).not.toBeInTheDocument();
    await userEvent.click(within(dialog).getByRole('button', { name: 'Close' }));
    await userEvent.click(within(rows[0] as HTMLElement).getByTestId('source-delete'));
    await userEvent.click(await screen.findByTestId('source-purge'));
    await userEvent.click(await screen.findByTestId('confirm-button'));
    await waitFor(() => expect(calls.find((c) => c.method === 'DELETE')?.path).toBe(`/__admin/capture/sources/${SRC_ID}?purge=true`));
  });

  it('gives a reader view-only sources', async () => {
    server({ principal: VIEWER, route: routes({}) });
    renderApp(<AppRoutes />, '/messages/sources');
    const rows = await screen.findAllByTestId('source-row');
    expect(screen.getByTestId('source-add')).toBeDisabled();
    expect(within(rows[0] as HTMLElement).queryByTestId('source-delete')).not.toBeInTheDocument();
    await userEvent.click(within(rows[0] as HTMLElement).getByTestId('source-edit'));
    const dialog = await screen.findByRole('dialog', { name: 'orders' });
    expect(within(dialog).getByTestId('source-read-only')).toHaveTextContent('messages:write');
  });
});

describe('async mocks on MQTT and AMQP', () => {
  it('shows the protocol, the AMQP exchange and routing key, and the trigger result', async () => {
    const amqp = { id: '01a1fb9c-0000-7000-8000-00000000b001', name: 'invoice-paid', protocol: 'amqp', channel: 'invoice.paid', amqp: { exchange: 'billing', persistent: true }, messages: [{ payload: { id: 1 } }], frequencyMs: 0, enabled: true };
    const mqtt = { id: '01a1fb9c-0000-7000-8000-00000000b002', name: 'telemetry', protocol: 'mqtt', channel: 'devices/7/telemetry', mqtt: { qos: 1, retain: true }, messages: [{ payload: { t: 20 } }], frequencyMs: 5000, enabled: true };
    server({
      route: (method, url) => {
        if (url.pathname === '/__admin/async-mocks' && method === 'GET') return { body: { asyncMocks: [amqp, mqtt] } };
        if (url.pathname === `/__admin/async-mocks/${amqp.id}/trigger`) return { body: { id: amqp.id, protocol: 'amqp', topic: 'invoice.paid', exchange: 'billing' } };
        return undefined;
      },
    });
    renderApp(<AppRoutes />, '/async');
    const rows = await screen.findAllByTestId('async-row');
    expect(rows[0]).toHaveTextContent('AMQP');
    expect(within(rows[0] as HTMLElement).getByTestId('async-destination')).toHaveTextContent('billing → invoice.paid');
    expect(rows[0]).toHaveTextContent('persistent');
    expect(rows[1]).toHaveTextContent('MQTT');
    expect(rows[1]).toHaveTextContent('QoS 1 · retained');
    await userEvent.click(within(rows[0] as HTMLElement).getByRole('button', { name: 'Trigger invoice-paid' }));
    expect(await screen.findByText('Published to billing → invoice.paid')).toBeInTheDocument();
  });
});
