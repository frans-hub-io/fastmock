import { Badge, Group, Stack, Table, Text } from '@mantine/core';
import type { JournalEntry, RequestPattern, ResponseDef } from '../api/types';
import type { components } from '../api/schema';
import { grpcCodeName, GRPC_KINDS } from '../lib/grpc';
import { BodyView, KeyValueTable, ToneBadge } from './common';

type S = components['schemas'];
type GrpcStatus = S['GrpcStatus'];
type GrpcMessage = S['GrpcMessage'];

function GrpcStatusBadge({ code, message }: { code: number | string | null | undefined; message?: string | null }) {
  const name = grpcCodeName(code);
  return (
    <Group gap="xs" wrap="wrap">
      <ToneBadge tone={name === 'OK' ? 'success' : 'danger'} testId="grpc-status">
        {name}
      </ToneBadge>
      {message && (
        <Text size="sm" className="mono">
          {message}
        </Text>
      )}
    </Group>
  );
}

function compact(v: unknown): string {
  return JSON.stringify(v ?? null);
}

/** A numbered list of JSON messages. */
export function MessageList({ messages, testId }: { messages: unknown[]; testId?: string }) {
  if (!messages.length)
    return (
      <Text size="sm" c="dimmed">
        no messages
      </Text>
    );
  return (
    <Stack gap={6} data-testid={testId}>
      {messages.map((m, i) => (
        <div key={i}>
          <Text size="xs" c="dimmed">
            #{i}
          </Text>
          <BodyView body={typeof m === 'string' ? m : JSON.stringify(m)} contentType="application/json" />
        </div>
      ))}
    </Stack>
  );
}

function parseList(text: string | null | undefined): unknown[] | null {
  if (!text) return [];
  try {
    const v = JSON.parse(text) as unknown;
    return Array.isArray(v) ? (v as unknown[]) : [v];
  } catch {
    return null;
  }
}

/** The gRPC side of a journal entry: kind, status and the messages, request or response side. */
export function GrpcCallView({ entry, side }: { entry: JournalEntry; side: 'request' | 'response' }) {
  const g = entry.grpc;
  if (!g) return null;
  const text = side === 'request' ? entry.body : entry.responseBody;
  const list = parseList(text);
  const total = side === 'request' ? g.requestMessages : g.responseMessages;
  const truncated = side === 'request' ? g.requestTruncated : g.responseTruncated;
  return (
    <Stack gap="sm" data-testid={`grpc-${side}`}>
      <Group gap="xs" wrap="wrap">
        <Badge variant="light" tt="none">
          gRPC {GRPC_KINDS[g.kind] ?? g.kind}
          {g.protocol ? ` · ${g.protocol}` : ''}
        </Badge>
        <Badge variant="outline" color="gray" tt="none">
          {total} message{total === 1 ? '' : 's'}
        </Badge>
        {truncated && (
          <ToneBadge tone="warning" testId={`grpc-${side}-truncated`}>
            journal shows the first {list?.length ?? 0}
          </ToneBadge>
        )}
      </Group>
      {side === 'response' && <GrpcStatusBadge code={g.status} message={g.message} />}
      {list ? <MessageList messages={list} /> : <BodyView body={text} contentType="application/json" />}
    </Stack>
  );
}

function messageRows(list: GrpcMessage[] | undefined, testId: string) {
  if (!list?.length) return null;
  return (
    <Table fz="sm" verticalSpacing={4} withTableBorder data-testid={testId}>
      <Table.Thead>
        <Table.Tr>
          <Table.Th w={40}>#</Table.Th>
          <Table.Th w={90}>delay</Table.Th>
          <Table.Th>message</Table.Th>
        </Table.Tr>
      </Table.Thead>
      <Table.Tbody>
        {list.map((m, i) => (
          <Table.Tr key={i}>
            <Table.Td>{i}</Table.Td>
            <Table.Td>{m.delayMs ? `${m.delayMs} ms` : '—'}</Table.Td>
            <Table.Td className="mono" style={{ wordBreak: 'break-all' }}>
              {compact(m.jsonBody)}
            </Table.Td>
          </Table.Tr>
        ))}
      </Table.Tbody>
    </Table>
  );
}

function StatusLine({ status }: { status: GrpcStatus | null | undefined }) {
  if (!status) return <GrpcStatusBadge code={0} />;
  return (
    <Stack gap={4}>
      <GrpcStatusBadge code={status.code} message={status.message} />
      {status.details?.map((d, i) => {
        const t = (d as Record<string, unknown>)['@type'];
        return (
          <Text key={i} size="xs" c="dimmed" className="mono">
            detail {typeof t === 'string' ? t : ''}
          </Text>
        );
      })}
    </Stack>
  );
}

/** A read-only summary of the gRPC parts of a stub (`request.grpc`, `response.grpc`). */
export function GrpcStubSummary({ request, response }: { request: RequestPattern; response: ResponseDef | undefined }) {
  const m = request.grpc;
  const g = response?.grpc;
  if (!m && !g) return null;
  const count = m?.messageCount;
  const limits = [
    g?.repeat ? 'repeat' : null,
    g?.maxMessages ? `max ${g.maxMessages} messages` : null,
    g?.closeAfterMs ? `ends after ${g.closeAfterMs} ms` : null,
    g?.compression ? `compression ${g.compression}` : null,
  ].filter(Boolean);
  return (
    <Stack gap="sm" data-testid="grpc-summary">
      <Text size="xs" c="dimmed">
        gRPC stream settings; edit them in the JSON tab.
      </Text>
      {m && (
        <Group gap="xs" wrap="wrap" data-testid="grpc-request">
          {count && (
            <Badge variant="outline" color="gray" tt="none">
              messages {count.equalTo != null ? `= ${count.equalTo}` : `${count.min ?? 0}…${count.max ?? '∞'}`}
            </Badge>
          )}
          {!!m.everyMessage?.length && (
            <Badge variant="outline" color="gray" tt="none">
              every message: {m.everyMessage.length} matcher{m.everyMessage.length === 1 ? '' : 's'}
            </Badge>
          )}
          {!!m.anyMessage?.length && (
            <Badge variant="outline" color="gray" tt="none">
              any message: {m.anyMessage.length}
            </Badge>
          )}
          {!!m.lastMessage?.length && (
            <Badge variant="outline" color="gray" tt="none">
              last message: {m.lastMessage.length}
            </Badge>
          )}
        </Group>
      )}
      {g && (
        <>
          {messageRows(g.messages, 'grpc-messages')}
          {limits.length > 0 && (
            <Text size="sm" c="dimmed">
              {limits.join(' · ')}
            </Text>
          )}
          <StatusLine status={g.status} />
          {g.trailers && Object.keys(g.trailers).length > 0 && <KeyValueTable data={g.trailers} />}
          {!!g.onMessage?.length && (
            <Stack gap={4} data-testid="grpc-rules">
              <Text size="sm" fw={500}>
                Replies to incoming messages
              </Text>
              {g.onMessage.map((r, i) => (
                <Text key={i} size="sm">
                  {r.match?.length ? `${r.match.length} matcher${r.match.length === 1 ? '' : 's'}` : 'any message'} →{' '}
                  {r.messages?.length ?? 0} message{(r.messages?.length ?? 0) === 1 ? '' : 's'}
                  {r.status ? `, then ${grpcCodeName(r.status.code)}` : ''}
                </Text>
              ))}
            </Stack>
          )}
        </>
      )}
    </Stack>
  );
}
