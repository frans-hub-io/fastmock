import { ActionIcon, Badge, Button, Drawer, Group, Paper, Stack, Switch, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { IconBolt, IconCode, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { AsyncMock } from '../api/types';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { PageHeader, Time } from '../components/common';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { pretty } from '../lib/format';

function AsyncInner() {
  const can = useCan();
  const canEdit = can('stubs:write', null);
  const [tag, setTag] = useState('');
  const [edit, setEdit] = useState<AsyncMock | null>(null);
  const [text, setText] = useState('');
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

  return (
    <Stack gap="md">
      <PageHeader title="Async mocks" description="Event-driven mocks imported from AsyncAPI: messages published periodically to Kafka topics or pushed over WebSocket/SSE." />
      <TextInput placeholder="Filter by tag" value={tag} onChange={(e) => setTag(e.currentTarget.value)} maw={300} aria-label="Tag" />
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data?.asyncMocks.length ? (
        <EmptyState title="No async mocks">Import an AsyncAPI document from the Stubs page to create them.</EmptyState>
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm">
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>On</Table.Th>
                  <Table.Th>Name</Table.Th>
                  <Table.Th>Protocol</Table.Th>
                  <Table.Th>Channel</Table.Th>
                  <Table.Th>Every</Table.Th>
                  <Table.Th>Messages</Table.Th>
                  <Table.Th>Updated</Table.Th>
                  <Table.Th />
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {q.data.asyncMocks.map((m) => (
                  <Table.Tr key={m.id}>
                    <Table.Td>
                      <Switch checked={m.enabled} disabled={!canEdit} onChange={(e) => put.mutate({ ...m, enabled: e.currentTarget.checked })} aria-label={`Enable ${m.name}`} size="sm" />
                    </Table.Td>
                    <Table.Td fw={500}>{m.name}</Table.Td>
                    <Table.Td>
                      <Badge variant="light">{m.protocol}</Badge>
                    </Table.Td>
                    <Table.Td className="mono">{m.channel}</Table.Td>
                    <Table.Td>{m.frequencyMs ? `${m.frequencyMs / 1000}s` : 'on trigger'}</Table.Td>
                    <Table.Td>{m.messages.length}</Table.Td>
                    <Table.Td>
                      <Time at={m.updatedAt} />
                    </Table.Td>
                    <Table.Td>
                      <Group gap={4} wrap="nowrap" justify="flex-end">
                        {canEdit && (
                          <Tooltip label="Publish one message now">
                            <ActionIcon variant="subtle" aria-label={`Trigger ${m.name}`} onClick={() => trigger.mutate(m)} loading={trigger.isPending && trigger.variables.id === m.id}>
                              <IconBolt size={16} />
                            </ActionIcon>
                          </Tooltip>
                        )}
                        <Tooltip label={canEdit ? 'Edit JSON' : 'View JSON'}>
                          <ActionIcon
                            variant="subtle"
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
                          <ActionIcon variant="subtle" color="red" aria-label={`Delete ${m.name}`} onClick={() => confirmAction({ title: 'Delete async mock?', message: m.name, confirm: 'Delete', onConfirm: () => del.mutate(m) })}>
                            <IconTrash size={16} />
                          </ActionIcon>
                        )}
                      </Group>
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
      <Drawer opened={!!edit} onClose={() => setEdit(null)} position="right" size="xl" title={edit?.name}>
        {edit && (
          <Stack gap="sm">
            <CodeEditor value={text} onChange={setText} readOnly={!canEdit} minHeight={400} label="Async mock JSON" />
            {canEdit && (
              <Group>
                <Button
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
            <Text size="xs" c="dimmed">
              Payloads may use templates; messages rotate in order.
            </Text>
          </Stack>
        )}
      </Drawer>
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
