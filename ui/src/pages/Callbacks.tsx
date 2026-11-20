import { Button, Drawer, Group, SegmentedControl, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconSearch, IconTrash, IconWebhook } from '@tabler/icons-react';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { CallbackJob } from '../api/types';
import { useCan, useSpace, useSpaceQuery } from '../auth/useAuth';
import { BodyView, DataTable, DensityToggle, KeyValueTable, MethodBadge, PageHeader, PathText, StatusBadge, Time, ToneBadge } from '../components/common';
import { docsUrl } from '../lib/docs';
import { useDensity, type Tone } from '../lib/ui';
import { SpaceBadge } from '../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { formatAbsolute, headerValue } from '../lib/format';

const STATUS_TONE: Record<string, Tone> = { pending: 'info', sent: 'success', failed: 'danger' };

export default function Callbacks() {
  const canEdit = useCan()('callbacks:delete');
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [params, setParams] = useSearchParams();
  const status = params.get('status') ?? 'all';
  const [correlation, setCorrelation] = useState(params.get('correlation') ?? '');
  const [source, setSource] = useState(params.get('source') ?? '');
  const [dCorr] = useDebouncedValue(correlation, 300);
  const [dSource] = useDebouncedValue(source, 300);
  const [selected, setSelected] = useState<CallbackJob | null>(null);
  const [density, setDensity] = useDensity();

  const query = { status: status === 'all' ? undefined : status, correlation: dCorr || undefined, source: dSource || undefined, limit: 500, ...spaceQ };
  const q = useQuery({
    queryKey: ['callbacks', 'list', query],
    queryFn: () => unwrap(client.GET('/__admin/callbacks', { params: { query } })),
    refetchInterval: 3000,
    placeholderData: keepPreviousData,
  });
  const clear = useApiMutation<void, { deleted: number }>({
    fn: () => unwrap(client.DELETE('/__admin/callbacks', { params: { query: spaceQ } })),
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
              variant="subtle"
              color="red"
              leftSection={<IconTrash size={16} />}
              onClick={() => confirmAction({ title: spaces.current ? `Delete all callbacks of space ${spaces.current}?` : 'Delete all callbacks?', message: 'Pending deliveries are cancelled and history is removed.', confirm: 'Delete all', onConfirm: () => clear.mutate() })}
            >
              Clear outbox
            </Button>
          )
        }
      />
      <div className="fm-toolbar">
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
        <TextInput placeholder="Correlation" leftSection={<IconSearch size={16} />} value={correlation} onChange={(e) => setCorrelation(e.currentTarget.value)} aria-label="Correlation" className="fm-grow" />
        <TextInput placeholder="Source" value={source} onChange={(e) => setSource(e.currentTarget.value)} aria-label="Source" w={200} />
        <DensityToggle value={density} onChange={setDensity} />
      </div>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data?.callbacks.length ? (
        <EmptyState
          title={status === 'all' && !dCorr && !dSource ? 'No callbacks yet' : 'No callbacks match the filters'}
          icon={<IconWebhook size={30} />}
          docs={{ label: 'Callbacks and webhooks guide', href: docsUrl('guide/callbacks') }}
        >
          Stubs with a callbacks section schedule webhook deliveries here; each one is retried with backoff until it succeeds.
        </EmptyState>
      ) : (
        <DataTable
          miw={1000}
          density={density}
          testId="callbacks-table"
          footer={
            <Text size="xs" c="dimmed">
              {q.data.callbacks.length} deliver{q.data.callbacks.length === 1 ? 'y' : 'ies'}
            </Text>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Created</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th>Target</Table.Th>
              <Table.Th>Attempts</Table.Th>
              <Table.Th>Last result</Table.Th>
              <Table.Th>Due</Table.Th>
              <Table.Th>Correlation</Table.Th>
              {spaces.enabled && <Table.Th>Space</Table.Th>}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {q.data.callbacks.map((c) => (
              <Table.Tr
                key={c.id}
                className={`clickable-row ${selected?.id === c.id ? 'fm-row-selected' : ''}`}
                tabIndex={0}
                onClick={() => setSelected(c)}
                onKeyDown={(e) => e.key === 'Enter' && setSelected(c)}
                data-testid="callback-row"
              >
                <Table.Td data-label="Created">
                  <Time at={c.createdAt} />
                </Table.Td>
                <Table.Td data-label="Status">
                  <ToneBadge tone={STATUS_TONE[c.status] ?? 'neutral'}>{c.status}</ToneBadge>
                </Table.Td>
                <Table.Td maw={380}>
                  <Group gap={8} wrap="nowrap">
                    <MethodBadge method={c.method} />
                    <PathText path={c.url} />
                  </Group>
                </Table.Td>
                <Table.Td data-label="Attempts" className="tabular">
                  {c.attempts}/{c.maxAttempts}
                </Table.Td>
                <Table.Td maw={260} data-label="Last result">
                  {c.lastError ? (
                    <Tooltip label={c.lastError} multiline w={360}>
                      <Group gap={6} wrap="nowrap" miw={0}>
                        {c.lastStatus ? <StatusBadge status={c.lastStatus} /> : null}
                        <Text size="sm" c="var(--fh-semantic-danger-fg)" className="truncate">
                          {c.lastError}
                        </Text>
                      </Group>
                    </Tooltip>
                  ) : c.lastStatus ? (
                    <StatusBadge status={c.lastStatus} />
                  ) : (
                    <Text span c="dimmed">
                      —
                    </Text>
                  )}
                </Table.Td>
                <Table.Td data-label="Due">
                  <Time at={c.dueAt} />
                </Table.Td>
                <Table.Td className="mono" data-label="Correlation">
                  {c.correlation ?? '—'}
                </Table.Td>
                {spaces.enabled && (
                  <Table.Td data-label="Space">
                    <SpaceBadge space={c.space} />
                  </Table.Td>
                )}
              </Table.Tr>
            ))}
          </Table.Tbody>
        </DataTable>
      )}
      <Drawer opened={!!selected} onClose={() => setSelected(null)} position="right" size="lg" title="Callback delivery">
        {selected && (
          <Stack gap="sm">
            <Group gap="xs">
              <MethodBadge method={selected.method} />
              <ToneBadge tone={STATUS_TONE[selected.status] ?? 'neutral'}>{selected.status}</ToneBadge>
            </Group>
            <Text size="sm" component="div">
              <PathText path={selected.url} />
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
