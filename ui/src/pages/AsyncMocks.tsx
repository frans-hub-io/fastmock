import { ActionIcon, Alert, Button, Drawer, Group, Stack, Switch, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { IconAlertTriangle, IconBolt, IconBroadcast, IconCode, IconDeviceFloppy, IconSearch, IconTrash, IconUpload } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { AsyncMock } from '../api/types';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { DataTable, DensityToggle, PageHeader, Time } from '../components/common';
import { ImportModal } from '../components/ImportModal';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { pretty } from '../lib/format';
import { frequencyLabel } from '../lib/screens';
import { useDensity } from '../lib/ui';

/** Where an async mock sends: a Kafka topic, or a WebSocket/SSE path. */
function Protocol({ protocol }: { protocol: string }) {
  const p = protocol.toLowerCase();
  return (
    <span className="fm-kind" data-kind={p === 'kafka' ? 'event' : p === 'ws' || p === 'websocket' ? 'grpc' : 'generic'}>
      {p === 'ws' ? 'WebSocket' : p === 'sse' ? 'SSE' : p === 'kafka' ? 'Kafka' : protocol}
    </span>
  );
}

function jsonProblem(text: string): string | null {
  try {
    const v = JSON.parse(text) as unknown;
    return v && typeof v === 'object' && !Array.isArray(v) ? null : 'An async mock is a JSON object';
  } catch (e) {
    return e instanceof Error ? e.message : String(e);
  }
}

function AsyncInner() {
  const can = useCan();
  const canEdit = can('stubs:write', null);
  const canImport = can('imports:run', null);
  const [tag, setTag] = useState('');
  const [edit, setEdit] = useState<AsyncMock | null>(null);
  const [text, setText] = useState('');
  const [importOpen, setImportOpen] = useState(false);
  const [density, setDensity] = useDensity();
  const q = useQuery({
    queryKey: ['async-mocks', tag],
    queryFn: () => unwrap(client.GET('/__admin/async-mocks', { params: { query: { tag: tag || undefined } } })),
    refetchInterval: 10000,
  });
  const put = useApiMutation<AsyncMock>({ fn: (m) => unwrap(client.PUT('/__admin/async-mocks/{id}', { params: { path: { id: m.id ?? '' } }, body: m })), invalidate: [['async-mocks']], success: (_d, m) => `Saved ${m.name}` });
  const del = useApiMutation<AsyncMock>({ fn: (m) => unwrap(client.DELETE('/__admin/async-mocks/{id}', { params: { path: { id: m.id ?? '' } } })), invalidate: [['async-mocks']], success: (_d, m) => `Deleted ${m.name}` });
  const trigger = useApiMutation<AsyncMock, { topic: string; partition: number; offset: number }>({
    fn: (m) => unwrap(client.POST('/__admin/async-mocks/{id}/trigger', { params: { path: { id: m.id ?? '' } } })),
    success: (d) => `Published to ${d.topic} [${d.partition}] @ ${d.offset}`,
  });
  const mocks = q.data?.asyncMocks ?? [];
  const on = mocks.filter((m) => m.enabled).length;
  const problem = edit && canEdit ? jsonProblem(text) : null;
  const dirty = !!edit && text !== pretty(edit);

  return (
    <Stack gap="md">
      <PageHeader
        title="Async mocks"
        description="Event-driven mocks imported from AsyncAPI: messages published periodically to Kafka topics or pushed over WebSocket/SSE."
        meta={
          mocks.length > 0 && (
            <>
              <span className="fm-status" data-tone={on ? 'success' : 'neutral'}>
                {on} publishing
              </span>
              <span className="fm-chip">{mocks.length - on} off</span>
            </>
          )
        }
        actions={
          canImport && (
            <Button variant="default" leftSection={<IconUpload size={16} />} onClick={() => setImportOpen(true)}>
              Import AsyncAPI
            </Button>
          )
        }
      />
      <div className="fm-toolbar">
        <TextInput placeholder="Filter by tag" leftSection={<IconSearch size={16} />} value={tag} onChange={(e) => setTag(e.currentTarget.value)} aria-label="Tag" className="fm-grow" classNames={{ input: 'mono' }} />
        <DensityToggle value={density} onChange={setDensity} />
      </div>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !mocks.length ? (
        tag ? (
          <EmptyState
            title="No async mocks carry this tag"
            icon={<IconSearch size={28} />}
            action={
              <Button variant="default" onClick={() => setTag('')}>
                Clear filter
              </Button>
            }
          />
        ) : (
          <EmptyState
            title="No async mocks"
            icon={<IconBroadcast size={30} />}
            docs={{ label: 'AsyncAPI guide', href: docsUrl('guide/asyncapi') }}
            action={
              canImport ? (
                <Button leftSection={<IconUpload size={16} />} onClick={() => setImportOpen(true)}>
                  Import an AsyncAPI document
                </Button>
              ) : undefined
            }
          >
            Importing an AsyncAPI document creates one async mock per operation the application sends, with the examples as messages.
          </EmptyState>
        )
      ) : (
        <DataTable
          testId="async-table"
          miw={980}
          density={density}
          footer={
            <Text size="xs" c="dimmed">
              {mocks.length} async mock{mocks.length === 1 ? '' : 's'} · refreshed every 10 seconds
            </Text>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th w={64}>On</Table.Th>
              <Table.Th>Name</Table.Th>
              <Table.Th>Protocol</Table.Th>
              <Table.Th>Channel</Table.Th>
              <Table.Th>Schedule</Table.Th>
              <Table.Th ta="right">Messages</Table.Th>
              <Table.Th>Updated</Table.Th>
              <Table.Th w={120}>
                <span className="sr-only">Actions</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {mocks.map((m) => (
              <Table.Tr key={m.id} data-testid="async-row" data-disabled={!m.enabled || undefined}>
                <Table.Td data-label="Enabled">
                  <Switch checked={m.enabled} disabled={!canEdit} onChange={(e) => put.mutate({ ...m, enabled: e.currentTarget.checked })} aria-label={`Enable ${m.name}`} size="sm" />
                </Table.Td>
                <Table.Td>
                  <Stack gap={2} miw={0}>
                    <Text size="sm" fw={600} c={m.enabled ? undefined : 'dimmed'} className="break-all">
                      {m.name}
                    </Text>
                    {m.service && (
                      <Text size="xs" c="dimmed" className="truncate">
                        {m.service}
                      </Text>
                    )}
                  </Stack>
                </Table.Td>
                <Table.Td data-label="Protocol">
                  <Protocol protocol={m.protocol ?? 'kafka'} />
                </Table.Td>
                <Table.Td data-label="Channel">
                  <span className="fm-path break-all">{m.channel}</span>
                </Table.Td>
                <Table.Td data-label="Schedule">
                  <span className="fm-chip">{frequencyLabel(m.frequencyMs)}</span>
                </Table.Td>
                <Table.Td data-label="Messages" ta="right" className="tabular">
                  {m.messages.length}
                </Table.Td>
                <Table.Td data-label="Updated">
                  <Time at={m.updatedAt} />
                </Table.Td>
                <Table.Td>
                  <Group gap={2} wrap="nowrap" justify="flex-end">
                    {canEdit && (
                      <Tooltip label="Publish one message now">
                        <ActionIcon variant="subtle" color="gray" aria-label={`Trigger ${m.name}`} onClick={() => trigger.mutate(m)} loading={trigger.isPending && trigger.variables.id === m.id}>
                          <IconBolt size={16} />
                        </ActionIcon>
                      </Tooltip>
                    )}
                    <Tooltip label={canEdit ? 'Edit JSON' : 'View JSON'}>
                      <ActionIcon
                        variant="subtle"
                        color="gray"
                        aria-label={`Edit ${m.name}`}
                        onClick={() => {
                          setEdit(m);
                          setText(pretty(m));
                        }}
                      >
                        <IconCode size={16} />
                      </ActionIcon>
                    </Tooltip>
                    {canEdit && (
                      <Tooltip label="Delete">
                        <ActionIcon variant="subtle" color="red" aria-label={`Delete ${m.name}`} onClick={() => confirmAction({ title: 'Delete async mock?', message: m.name, confirm: 'Delete', onConfirm: () => del.mutate(m) })}>
                          <IconTrash size={16} />
                        </ActionIcon>
                      </Tooltip>
                    )}
                  </Group>
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </DataTable>
      )}
      <Drawer opened={!!edit} onClose={() => setEdit(null)} position="right" size="xl" title={edit?.name}>
        {edit && (
          <Stack gap="sm">
            <Group gap={6} wrap="wrap">
              <Protocol protocol={edit.protocol ?? 'kafka'} />
              <span className="fm-path">{edit.channel}</span>
              <span className="fm-chip">{frequencyLabel(edit.frequencyMs)}</span>
            </Group>
            <CodeEditor value={text} onChange={setText} readOnly={!canEdit} minHeight={400} label="Async mock JSON" />
            {problem && (
              <Alert color="red" icon={<IconAlertTriangle size={18} />} p="xs">
                {problem}
              </Alert>
            )}
            <Text size="xs" c="dimmed">
              Payloads may use templates; messages rotate in order.
            </Text>
            {canEdit && (
              <Group justify="flex-end" gap="xs">
                <Button variant="subtle" color="gray" onClick={() => setEdit(null)}>
                  Cancel
                </Button>
                <Button
                  leftSection={<IconDeviceFloppy size={16} />}
                  disabled={!!problem || !dirty}
                  onClick={() => {
                    try {
                      put.mutate({ ...(JSON.parse(text) as AsyncMock), id: edit.id ?? '' });
                      setEdit(null);
                    } catch {
                      /* the editor shows the syntax error */
                    }
                  }}
                >
                  Save
                </Button>
              </Group>
            )}
          </Stack>
        )}
      </Drawer>
      <ImportModal opened={importOpen} onClose={() => setImportOpen(false)} />
    </Stack>
  );
}

export default function AsyncMocks() {
  return (
    <FeatureGate path="/__admin/async-mocks" method="get" what="Async mocks">
      <AsyncInner />
    </FeatureGate>
  );
}
