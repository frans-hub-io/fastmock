import { Alert, Button, Group, NumberInput, Stack, Table, Text, Textarea, TextInput } from '@mantine/core';
import { IconDatabase, IconDeviceFloppy, IconEdit, IconKey, IconList, IconSearch, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useFeature } from '../api/features';
import { api, isNotFound } from '../api/http';
import { confirmAction, useApiMutation } from '../api/mutations';
import { useCan, useSpace, useSpaceChoice } from '../auth/useAuth';
import { BodyView, CommandSnippet, PageHeader, SectionCard } from '../components/common';
import { SpaceField } from '../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { formatBytes, formatDuration } from '../lib/format';
import { utf8Bytes } from '../lib/screens';

interface KvEntry {
  key: string;
  value: string;
  ttlSeconds?: number | null;
}

const MAX_KEY = 512;
const MAX_VALUE = 1024 * 1024;

function KvList({ onPick, active }: { onPick: (k: string) => void; active: string | null }) {
  const [prefix, setPrefix] = useState('');
  const q = useQuery({
    queryKey: ['kv', 'list', prefix],
    queryFn: () => api.get<{ keys?: string[]; entries?: KvEntry[] }>('/__admin/kv', { query: { prefix: prefix || undefined, limit: 500 } }),
  });
  const keys = q.data?.keys ?? q.data?.entries?.map((e) => e.key) ?? [];
  return (
    <SectionCard title="Keys" description="Pick a key to look it up" icon={<IconList size={16} />}>
      <TextInput placeholder="Key prefix" leftSection={<IconSearch size={16} />} value={prefix} onChange={(e) => setPrefix(e.currentTarget.value)} mb="sm" aria-label="Key prefix" classNames={{ input: 'mono' }} />
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : !keys.length ? (
        <EmptyState title="No keys" compact icon={<IconKey size={20} />} />
      ) : (
        <Table fz="sm" className="fm-table">
          <Table.Tbody>
            {keys.map((k) => (
              <Table.Tr key={k} className={`clickable-row ${k === active ? 'fm-row-selected' : ''}`} tabIndex={0} onClick={() => onPick(k)} onKeyDown={(e) => e.key === 'Enter' && onPick(k)}>
                <Table.Td className="mono break-all">{k}</Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </Table>
      )}
    </SectionCard>
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
  const [recent, setRecent] = useState<string[]>([]);
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
    onSuccess: () => look(key),
  });
  const del = useApiMutation<string>({
    fn: (k) => api.del(`/__admin/kv/${encodeURIComponent(k)}`, { query: at }),
    invalidate: [['kv']],
    success: (_d, k) => `Deleted ${k}`,
  });
  const look = (k: string) => {
    setLookup(k);
    setRecent((r) => [k, ...r.filter((x) => x !== k)].slice(0, 8));
  };
  const pick = (k: string) => {
    setKey(k);
    look(k);
  };
  const bytes = utf8Bytes(value);
  const keyError = key.length > MAX_KEY ? `At most ${String(MAX_KEY)} characters` : undefined;
  const valueError = bytes > MAX_VALUE ? `${formatBytes(bytes)} is over the 1 MiB limit` : undefined;

  return (
    <Stack gap="md">
      <PageHeader title="KV store" description="Shared key/value pairs that templates read and write. Values are strings up to 1 MiB." />
      <div className="fm-aside-col">
        <Stack gap="md">
          <SectionCard title="Look up a key" description="Read the value stored under a key" icon={<IconSearch size={16} />}>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                if (key) look(key);
              }}
            >
              <Stack gap="sm">
                <SpaceField perm="state:read" value={area} onChange={setArea} label="Space" description="Keys are looked up, stored and deleted in this space" />
                <Group gap="xs" align="flex-end" wrap="wrap" mt={spaces.enabled ? 0 : undefined}>
                  <TextInput
                    label="Key"
                    placeholder="session:ada"
                    value={key}
                    onChange={(e) => setKey(e.currentTarget.value)}
                    className="fm-grow"
                    classNames={{ input: 'mono' }}
                    error={keyError}
                  />
                  <Button type="submit" leftSection={<IconSearch size={16} />} variant="light" disabled={!key || !!keyError} mb={keyError ? 22 : 0}>
                    Look up
                  </Button>
                </Group>
                {recent.length > 0 && (
                  <Group gap={4} wrap="wrap" aria-label="Recent lookups">
                    <Text size="xs" c="dimmed" mr={2}>
                      Recent
                    </Text>
                    {recent.map((k) => (
                      <button key={k} type="button" className="fm-chip" onClick={() => pick(k)}>
                        {k}
                      </button>
                    ))}
                  </Group>
                )}
              </Stack>
            </form>
            {lookup && (
              <Stack mt="md" gap="xs" data-testid="kv-entry">
                {entry.isLoading ? (
                  <LoadingState />
                ) : entry.isError ? (
                  isNotFound(entry.error) ? (
                    <Alert color="gray" icon={<IconKey size={18} />}>
                      No value stored under <span className="mono">{lookup}</span>.
                    </Alert>
                  ) : (
                    <ErrorState error={entry.error} />
                  )
                ) : entry.data ? (
                  <>
                    <Group justify="space-between" wrap="wrap" gap="xs" className="fm-detail-head fm-head-box" p="xs">
                      <Group gap={6} wrap="wrap" miw={0}>
                        <span className="fm-section-icon" aria-hidden>
                          <IconKey size={16} />
                        </span>
                        <Text size="sm" fw={700} className="mono break-all">
                          {entry.data.key}
                        </Text>
                        <span className="fm-chip">{formatBytes(utf8Bytes(entry.data.value))}</span>
                        {entry.data.ttlSeconds ? <span className="fm-chip fm-chip-soft">expires in {formatDuration(entry.data.ttlSeconds)}</span> : <span className="fm-chip">no expiry</span>}
                      </Group>
                      {canEdit && (
                        <Group gap={4} wrap="nowrap">
                          <Button
                            size="xs"
                            variant="default"
                            leftSection={<IconEdit size={14} />}
                            onClick={() => {
                              setKey(entry.data.key);
                              setValue(entry.data.value);
                            }}
                          >
                            Edit this value
                          </Button>
                          <Button
                            size="xs"
                            color="red"
                            variant="subtle"
                            leftSection={<IconTrash size={14} />}
                            onClick={() => confirmAction({ title: 'Delete key?', message: `Delete “${lookup}”?`, confirm: 'Delete', onConfirm: () => del.mutate(lookup) })}
                          >
                            Delete
                          </Button>
                        </Group>
                      )}
                    </Group>
                    <BodyView body={entry.data.value} empty="empty string" />
                  </>
                ) : null}
              </Stack>
            )}
          </SectionCard>
          {canEdit && (
            <SectionCard title="Set a value" description={key ? <span className="mono">{key}</span> : 'Enter a key above'} icon={<IconDeviceFloppy size={16} />}>
              <form
                onSubmit={(e) => {
                  e.preventDefault();
                  if (key && !keyError && !valueError) put.mutate();
                }}
              >
                <Stack gap="sm">
                  <Textarea
                    label="Value"
                    description={`${formatBytes(bytes)} of 1 MiB`}
                    value={value}
                    onChange={(e) => setValue(e.currentTarget.value)}
                    autosize
                    minRows={4}
                    maxRows={14}
                    classNames={{ input: 'mono' }}
                    error={valueError}
                  />
                  <Group gap="sm" align="flex-end" justify="space-between" wrap="wrap">
                    <NumberInput label="TTL" description="Empty = no expiry" suffix=" s" value={ttl} onChange={(v) => setTtl(typeof v === 'number' ? v : '')} min={1} max={31536000} w={200} />
                    <Button type="submit" leftSection={<IconDeviceFloppy size={16} />} disabled={!key || !!keyError || !!valueError} loading={put.isPending}>
                      Save {key ? `“${key}”` : ''}
                    </Button>
                  </Group>
                </Stack>
              </form>
            </SectionCard>
          )}
        </Stack>
        {listFeature === 'available' ? (
          <KvList onPick={pick} active={lookup} />
        ) : (
          <SectionCard title="Using the store" description="This server does not list keys; look a key up by name." icon={<IconDatabase size={16} />}>
            <Stack gap="sm">
              <Text size="sm" c="dimmed">
                Templates and scripts read and write the same keys as this screen. To store a value from the command line:
              </Text>
              <CommandSnippet command={`curl -X PUT ${window.location.origin}/__admin/kv/${encodeURIComponent(key || 'feature-flags')} -H "Authorization: Bearer $FASTMOCK_TOKEN" -H 'content-type: application/json' -d '{"value":"on"}'`} />
              <Text size="xs" c="dimmed">
                Values are strings; store JSON as text and parse it where you read it. A TTL from 1 second to a year makes a key expire.
              </Text>
              <Group>
                <Button component="a" href={docsUrl('guide/scenarios-and-state')} target="_blank" rel="noopener noreferrer" variant="default" size="xs" leftSection={<IconDatabase size={14} />}>
                  Key-value store guide
                </Button>
              </Group>
            </Stack>
          </SectionCard>
        )}
      </div>
    </Stack>
  );
}
