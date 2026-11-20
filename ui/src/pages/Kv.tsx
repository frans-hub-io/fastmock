import { Alert, Button, Group, NumberInput, Paper, Stack, Table, Text, Textarea, TextInput, Title } from '@mantine/core';
import { IconSearch, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useFeature } from '../api/features';
import { api, isNotFound } from '../api/http';
import { confirmAction, useApiMutation } from '../api/mutations';
import { useCan, useSpace, useSpaceChoice } from '../auth/useAuth';
import { BodyView, PageHeader } from '../components/common';
import { SpaceField } from '../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../components/States';

interface KvEntry {
  key: string;
  value: string;
  ttlSeconds?: number | null;
}

function KvList({ onPick }: { onPick: (k: string) => void }) {
  const [prefix, setPrefix] = useState('');
  const q = useQuery({
    queryKey: ['kv', 'list', prefix],
    queryFn: () => api.get<{ keys?: string[]; entries?: KvEntry[] }>('/__admin/kv', { query: { prefix: prefix || undefined, limit: 500 } }),
  });
  const keys = q.data?.keys ?? q.data?.entries?.map((e) => e.key) ?? [];
  return (
    <Paper withBorder p="md" radius="md">
      <Title order={3} size="h5" mb="sm">
        Keys
      </Title>
      <TextInput placeholder="Key prefix" value={prefix} onChange={(e) => setPrefix(e.currentTarget.value)} mb="sm" aria-label="Key prefix" />
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : !keys.length ? (
        <EmptyState title="No keys" />
      ) : (
        <Table fz="sm" highlightOnHover>
          <Table.Tbody>
            {keys.map((k) => (
              <Table.Tr key={k} className="clickable-row" tabIndex={0} onClick={() => onPick(k)} onKeyDown={(e) => e.key === 'Enter' && onPick(k)}>
                <Table.Td className="mono">{k}</Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}
    </Paper>
  );
}

export default function Kv() {
  const spaces = useSpace();
  const [area, setArea] = useSpaceChoice('state:read');
  const canEdit = useCan()('state:write', area);
  const at = area ? { space: area } : undefined;
  const listFeature = useFeature('/__admin/kv', 'get');
  const [key, setKey] = useState('');
  const [lookup, setLookup] = useState<string | null>(null);
  const [value, setValue] = useState('');
  const [ttl, setTtl] = useState<number | ''>('');
  const entry = useQuery({
    queryKey: ['kv', 'one', lookup, area],
    queryFn: () => api.get<KvEntry>(`/__admin/kv/${encodeURIComponent(lookup ?? '')}`, { query: at }),
    enabled: !!lookup,
    retry: false,
  });
  const put = useApiMutation<void>({
    fn: () => api.put(`/__admin/kv/${encodeURIComponent(key)}`, { value, ...(ttl ? { ttlSeconds: ttl } : {}) }, { query: at }),
    invalidate: [['kv']],
    success: `Saved ${key}`,
    onSuccess: () => setLookup(key),
  });
  const del = useApiMutation<string>({
    fn: (k) => api.del(`/__admin/kv/${encodeURIComponent(k)}`, { query: at }),
    invalidate: [['kv']],
    success: (_d, k) => `Deleted ${k}`,
  });
  const pick = (k: string) => {
    setKey(k);
    setLookup(k);
  };

  return (
    <Stack gap="md">
      <PageHeader title="KV store" description="Shared key/value pairs that templates read and write. Values are strings up to 1 MiB." />
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(340px, 1fr))', gap: 'var(--mantine-spacing-lg)', alignItems: 'start' }}>
        <Stack gap="md">
          <Paper withBorder p="md" radius="md">
            <SpaceField perm="state:read" value={area} onChange={setArea} label="Space" description="Keys are looked up, stored and deleted in this space" />
            <Group gap="xs" align="flex-end" wrap="nowrap" mt={spaces.enabled ? 'sm' : 0}>
              <TextInput label="Key" value={key} onChange={(e) => setKey(e.currentTarget.value)} style={{ flex: 1 }} className="mono" maxLength={512} />
              <Button leftSection={<IconSearch size={16} />} variant="light" disabled={!key} onClick={() => setLookup(key)}>
                Look up
              </Button>
            </Group>
            {lookup && (
              <Stack mt="md" gap="xs">
                {entry.isLoading ? (
                  <LoadingState />
                ) : entry.isError ? (
                  isNotFound(entry.error) ? (
                    <Alert color="gray">No value stored under “{lookup}”.</Alert>
                  ) : (
                    <ErrorState error={entry.error} />
                  )
                ) : entry.data ? (
                  <>
                    <Group justify="space-between">
                      <Text size="sm" fw={600} className="mono">
                        {entry.data.key}
                      </Text>
                      {canEdit && (
                        <Button
                          size="xs"
                          color="red"
                          variant="subtle"
                          leftSection={<IconTrash size={14} />}
                          onClick={() => confirmAction({ title: 'Delete key?', message: `Delete “${lookup}”?`, confirm: 'Delete', onConfirm: () => del.mutate(lookup) })}
                        >
                          Delete
                        </Button>
                      )}
                    </Group>
                    <BodyView body={entry.data.value} />
                    {canEdit && (
                      <Button size="xs" variant="default" onClick={() => setValue(entry.data.value)}>
                        Edit this value
                      </Button>
                    )}
                  </>
                ) : null}
              </Stack>
            )}
          </Paper>
          {canEdit && (
            <Paper withBorder p="md" radius="md">
              <Title order={3} size="h5" mb="sm">
                Set value
              </Title>
              <Stack gap="xs">
                <Textarea label="Value" value={value} onChange={(e) => setValue(e.currentTarget.value)} autosize minRows={3} maxRows={14} classNames={{ input: 'mono' }} />
                <NumberInput label="TTL seconds" description="Empty = no expiry" value={ttl} onChange={(v) => setTtl(typeof v === 'number' ? v : '')} min={1} max={31536000} w={200} />
                <Group>
                  <Button disabled={!key} loading={put.isPending} onClick={() => put.mutate()}>
                    Save {key ? `“${key}”` : ''}
                  </Button>
                </Group>
              </Stack>
            </Paper>
          )}
        </Stack>
        {listFeature === 'available' ? (
          <KvList onPick={pick} />
        ) : (
          <Paper withBorder p="md" radius="md">
            <Text size="sm" c="dimmed">
              This server does not list keys; look a key up by name.
            </Text>
          </Paper>
        )}
      </div>
    </Stack>
  );
}
