import { Badge, Button, Drawer, Group, Paper, SegmentedControl, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconTrash } from '@tabler/icons-react';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { CallbackJob } from '../api/types';
import { useCan } from '../auth/useAuth';
import { BodyView, KeyValueTable, MethodBadge, PageHeader, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { formatAbsolute, headerValue } from '../lib/format';

const STATUS_COLOR: Record<string, string> = { pending: 'blue', sent: 'green', failed: 'red' };

export default function Callbacks() {
  const canEdit = useCan('editor');
  const [params, setParams] = useSearchParams();
  const status = params.get('status') ?? 'all';
  const [correlation, setCorrelation] = useState(params.get('correlation') ?? '');
  const [source, setSource] = useState(params.get('source') ?? '');
  const [dCorr] = useDebouncedValue(correlation, 300);
  const [dSource] = useDebouncedValue(source, 300);
  const [selected, setSelected] = useState<CallbackJob | null>(null);

  const query = { status: status === 'all' ? undefined : status, correlation: dCorr || undefined, source: dSource || undefined, limit: 500 };
  const q = useQuery({
    queryKey: ['callbacks', 'list', query],
    queryFn: () => unwrap(client.GET('/__admin/callbacks', { params: { query } })),
    refetchInterval: 3000,
    placeholderData: keepPreviousData,
  });
  const clear = useApiMutation<void, { deleted: number }>({
    fn: () => unwrap(client.DELETE('/__admin/callbacks')),
    invalidate: [['callbacks']],
    success: (d) => `Deleted ${d.deleted} callbacks`,
  });

  return (
    <Stack gap="md">
      <PageHeader
        title="Callbacks"
        description="The durable outbox of webhook deliveries that stubs schedule. Delivery is retried with backoff; signature headers are masked."
        actions={
          canEdit && (
            <Button
              variant="light"
              color="red"
              leftSection={<IconTrash size={16} />}
              onClick={() => confirmAction({ title: 'Delete all callbacks?', message: 'Pending deliveries are cancelled and history is removed.', confirm: 'Delete all', onConfirm: () => clear.mutate() })}
            >
              Clear outbox
            </Button>
          )
        }
      />
      <Paper withBorder p="sm" radius="md">
        <Group gap="sm" wrap="wrap">
          <SegmentedControl
            value={status}
            onChange={(v) => {
              const next = new URLSearchParams(params);
              if (v === 'all') next.delete('status');
              else next.set('status', v);
              setParams(next, { replace: true });
            }}
            data={['all', 'pending', 'sent', 'failed'].map((s) => ({ label: (s[0] ?? '').toUpperCase() + s.slice(1), value: s }))}
            aria-label="Status filter"
          />
          <TextInput placeholder="Correlation" value={correlation} onChange={(e) => setCorrelation(e.currentTarget.value)} aria-label="Correlation" />
          <TextInput placeholder="Source" value={source} onChange={(e) => setSource(e.currentTarget.value)} aria-label="Source" />
        </Group>
      </Paper>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data?.callbacks.length ? (
        <EmptyState title="No callbacks">Stubs with a callbacks section schedule deliveries here.</EmptyState>
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm" highlightOnHover miw={960}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Created</Table.Th>
                  <Table.Th>Status</Table.Th>
                  <Table.Th>Method</Table.Th>
                  <Table.Th>URL</Table.Th>
                  <Table.Th>Attempts</Table.Th>
                  <Table.Th>Last result</Table.Th>
                  <Table.Th>Due</Table.Th>
                  <Table.Th>Correlation</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {q.data.callbacks.map((c) => (
                  <Table.Tr key={c.id} className="clickable-row" tabIndex={0} onClick={() => setSelected(c)} onKeyDown={(e) => e.key === 'Enter' && setSelected(c)}>
                    <Table.Td>
                      <Time at={c.createdAt} />
                    </Table.Td>
                    <Table.Td>
                      <Badge color={STATUS_COLOR[c.status] ?? 'gray'} variant="light">
                        {c.status}
                      </Badge>
                    </Table.Td>
                    <Table.Td>
                      <MethodBadge method={c.method} />
                    </Table.Td>
                    <Table.Td className="mono" maw={340} style={{ wordBreak: 'break-all' }}>
                      {c.url}
                    </Table.Td>
                    <Table.Td>
                      {c.attempts}/{c.maxAttempts}
                    </Table.Td>
                    <Table.Td maw={260}>
                      {c.lastError ? (
                        <Tooltip label={c.lastError} multiline w={360}>
                          <Text size="sm" c="red" className="truncate">
                            {c.lastStatus ? `${c.lastStatus} · ` : ''}
                            {c.lastError}
                          </Text>
                        </Tooltip>
                      ) : c.lastStatus ? (
                        c.lastStatus
                      ) : (
                        '—'
                      )}
                    </Table.Td>
                    <Table.Td>
                      <Time at={c.dueAt} />
                    </Table.Td>
                    <Table.Td className="mono">{c.correlation ?? '—'}</Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
      <Drawer opened={!!selected} onClose={() => setSelected(null)} position="right" size="lg" title="Callback delivery">
        {selected && (
          <Stack gap="sm">
            <Group gap="xs">
              <Badge color={STATUS_COLOR[selected.status] ?? 'gray'}>{selected.status}</Badge>
              <MethodBadge method={selected.method} />
            </Group>
            <Text className="mono" size="sm" style={{ wordBreak: 'break-all' }}>
              {selected.url}
            </Text>
            <Table fz="sm" withTableBorder>
              <Table.Tbody>
                {(
                  [
                    ['Delivery id', selected.id],
                    ['Created', formatAbsolute(selected.createdAt)],
                    ['Due', formatAbsolute(selected.dueAt)],
                    ['Sent', selected.sentAt ? formatAbsolute(selected.sentAt) : '—'],
                    ['Attempts', `${selected.attempts} of ${selected.maxAttempts}`],
                    ['Last status', selected.lastStatus ?? '—'],
                    ['Last error', selected.lastError ?? '—'],
                    ['traceparent', selected.traceparent ?? '—'],
                    ['Source', selected.source],
                  ] as [string, string | number][]
                ).map(([k, v]) => (
                  <Table.Tr key={k}>
                    <Table.Td c="dimmed" w={130}>
                      {k}
                    </Table.Td>
                    <Table.Td className="mono" style={{ wordBreak: 'break-all' }}>
                      {v}
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
            <Text fw={600} size="sm">
              Headers
            </Text>
            <KeyValueTable data={selected.headers} />
            <Text fw={600} size="sm">
              Body
            </Text>
            <BodyView body={selected.body} contentType={headerValue(selected.headers, 'content-type')} />
          </Stack>
        )}
      </Drawer>
    </Stack>
  );
}
