import {
  Anchor,
  Badge,
  Button,
  Drawer,
  Group,
  NumberInput,
  Paper,
  SegmentedControl,
  Select,
  Stack,
  Table,
  Tabs,
  Text,
  TextInput,
  Tooltip,
} from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconPlayerPause, IconPlayerPlay, IconPlus, IconRefresh, IconSearch, IconTrash } from '@tabler/icons-react';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { JournalEntry } from '../api/types';
import { useCan, useSpace, useSpaceQuery } from '../auth/useAuth';
import { SpaceBadge } from '../components/spaces';
import { BodyView, Copy, KeyValueTable, MethodBadge, PageHeader, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { toCurl } from '../lib/curl';
import { formatAbsolute, headerValue } from '../lib/format';
import { stubFromJournal } from '../lib/stubs';
import { defaultDataPlaneBase, storedDataPlaneBase, useUiConfig } from '../api/uiConfig';

const METHODS = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'];

export function EntryDetail({ entry }: { entry: JournalEntry }) {
  const navigate = useNavigate();
  const canEdit = useCan()('stubs:write');
  const spaces = useSpace();
  const cfg = useUiConfig();
  const base = storedDataPlaneBase() ?? defaultDataPlaneBase(cfg.data);
  const curl = toCurl(base, { method: entry.method, path: entry.path, query: entry.query, headers: entry.headers, body: entry.body });
  return (
    <Stack gap="md">
      <Group gap="xs" wrap="wrap">
        <MethodBadge method={entry.method} />
        <StatusBadge status={entry.status} />
        <Badge color={entry.matched ? 'green' : 'orange'} variant="light">
          {entry.matched ? 'matched' : 'unmatched'}
        </Badge>
        <Badge variant="outline" color="gray">
          {entry.durationMs} ms
        </Badge>
        <Badge variant="outline" color="gray">
          {entry.source}
        </Badge>
      </Group>
      <Text className="mono" style={{ wordBreak: 'break-all' }}>
        {entry.path}
        {entry.query ? `?${entry.query}` : ''}
      </Text>
      <Table fz="sm" verticalSpacing={4} withTableBorder>
        <Table.Tbody>
          <Table.Tr>
            <Table.Td c="dimmed" w={130}>
              Received
            </Table.Td>
            <Table.Td>{formatAbsolute(entry.at)}</Table.Td>
          </Table.Tr>
          <Table.Tr>
            <Table.Td c="dimmed">Journal id</Table.Td>
            <Table.Td className="mono">{entry.id}</Table.Td>
          </Table.Tr>
          <Table.Tr>
            <Table.Td c="dimmed">Matched stub</Table.Td>
            <Table.Td>
              {entry.stubId ? (
                <Anchor component={Link} to={`/stubs/${entry.stubId}`} className="mono" size="sm">
                  {entry.stubId}
                </Anchor>
              ) : (
                '—'
              )}
            </Table.Td>
          </Table.Tr>
          <Table.Tr>
            <Table.Td c="dimmed">Trace id</Table.Td>
            <Table.Td className="mono">
              {entry.traceId ? (
                <Group gap={4} wrap="nowrap">
                  {entry.traceId}
                  <Copy value={entry.traceId} label="Copy trace id" />
                </Group>
              ) : (
                '—'
              )}
            </Table.Td>
          </Table.Tr>
          <Table.Tr>
            <Table.Td c="dimmed">Correlation</Table.Td>
            <Table.Td className="mono">{entry.correlation ?? '—'}</Table.Td>
          </Table.Tr>
          {spaces.enabled && (
            <Table.Tr>
              <Table.Td c="dimmed">Space</Table.Td>
              <Table.Td>
                <SpaceBadge space={entry.space} />
              </Table.Td>
            </Table.Tr>
          )}
        </Table.Tbody>
      </Table>
      <Group gap="xs">
        {canEdit && (
          <Button
            size="xs"
            leftSection={<IconPlus size={14} />}
            onClick={() => void navigate('/stubs/new', { state: { stub: { ...stubFromJournal(entry), ...(entry.space ? { space: entry.space } : {}) } } })}
            data-testid="stub-from-request"
          >
            Create stub from this request
          </Button>
        )}
        {!entry.matched && (
          <Button size="xs" variant="light" leftSection={<IconSearch size={14} />} component={Link} to={`/near-misses?id=${entry.id}`}>
            Near misses
          </Button>
        )}
        <Group gap={2}>
          <Text size="xs" c="dimmed">
            curl
          </Text>
          <Copy value={curl} label="Copy as curl" />
        </Group>
      </Group>
      <Tabs defaultValue="request">
        <Tabs.List>
          <Tabs.Tab value="request">Request</Tabs.Tab>
          <Tabs.Tab value="response">Response</Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="request" pt="sm">
          <Stack gap="sm">
            <Text size="sm" fw={600}>
              Headers
            </Text>
            <KeyValueTable data={entry.headers} />
            <Text size="sm" fw={600}>
              Body
            </Text>
            <BodyView body={entry.body} contentType={headerValue(entry.headers, 'content-type')} />
          </Stack>
        </Tabs.Panel>
        <Tabs.Panel value="response" pt="sm">
          <Stack gap="sm">
            <Group gap="xs">
              <Text size="sm" fw={600}>
                Status
              </Text>
              <StatusBadge status={entry.status} />
            </Group>
            <Text size="sm" fw={600}>
              Body
            </Text>
            <BodyView body={entry.responseBody} contentType={undefined} />
          </Stack>
        </Tabs.Panel>
      </Tabs>
    </Stack>
  );
}

export default function Journal() {
  const canEdit = useCan()('journal:delete');
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [params, setParams] = useSearchParams();
  const [live, setLive] = useState(true);
  const get = (k: string) => params.get(k) ?? '';
  const [pathPrefix, setPathPrefix] = useState(get('pathPrefix'));
  const [bodyContains, setBodyContains] = useState(get('bodyContains'));
  const [correlation, setCorrelation] = useState(get('correlation'));
  const [source, setSource] = useState(get('source'));
  const [dPath] = useDebouncedValue(pathPrefix, 300);
  const [dBody] = useDebouncedValue(bodyContains, 300);
  const [dCorr] = useDebouncedValue(correlation, 300);
  const [dSource] = useDebouncedValue(source, 300);
  const matched = params.get('matched') ?? 'all';
  const method = params.get('method');
  const since = get('since');
  const limit = Number(params.get('limit') ?? '200') || 200;
  const selectedId = params.get('id');

  const setParam = (k: string, v: string | null) => {
    const next = new URLSearchParams(params);
    if (v === null || v === '' || v === 'all') next.delete(k);
    else next.set(k, v);
    setParams(next, { replace: true });
  };

  const query = {
    matched: matched === 'all' ? undefined : matched === 'matched',
    method: method ?? undefined,
    pathPrefix: dPath || undefined,
    bodyContains: dBody || undefined,
    correlation: dCorr || undefined,
    source: dSource || undefined,
    since: since ? new Date(since).toISOString() : undefined,
    limit,
    ...spaceQ,
  };
  const list = useQuery({
    queryKey: ['requests', 'list', query],
    queryFn: () => unwrap(client.GET('/__admin/requests', { params: { query } })),
    refetchInterval: live ? 2000 : false,
    placeholderData: keepPreviousData,
  });
  const selectedFromList = list.data?.requests.find((r) => String(r.id) === selectedId);
  const single = useQuery({
    queryKey: ['requests', 'one', selectedId],
    queryFn: () => unwrap(client.GET('/__admin/requests/{id}', { params: { path: { id: Number(selectedId) } } })),
    enabled: !!selectedId && !selectedFromList,
    retry: false,
  });
  const selected = selectedFromList ?? single.data;

  const clear = useApiMutation<void, { deleted: number }>({
    fn: () => unwrap(client.DELETE('/__admin/requests', { params: { query: spaceQ } })),
    invalidate: [['requests']],
    success: (d) => `Cleared ${d.deleted} journal entries`,
  });

  return (
    <Stack gap="md">
      <PageHeader
        title="Request journal"
        description="Every request the mock port received, newest first. Sensitive headers and JSON keys are redacted."
        actions={
          <>
            <Button
              variant={live ? 'light' : 'default'}
              leftSection={live ? <IconPlayerPause size={16} /> : <IconPlayerPlay size={16} />}
              onClick={() => setLive((v) => !v)}
              data-testid="live-toggle"
            >
              {live ? 'Pause' : 'Resume'}
            </Button>
            <Tooltip label="Refresh now">
              <Button variant="default" onClick={() => void list.refetch()} aria-label="Refresh" px="xs">
                <IconRefresh size={16} />
              </Button>
            </Tooltip>
            {canEdit && (
              <Button
                variant="light"
                color="red"
                leftSection={<IconTrash size={16} />}
                onClick={() =>
                  confirmAction({
                    title: spaces.current ? `Clear the journal of space ${spaces.current}?` : 'Clear the request journal?',
                    message: 'All journal entries are deleted for every replica. Verification against them will no longer be possible.',
                    confirm: 'Clear journal',
                    onConfirm: () => clear.mutate(),
                  })
                }
              >
                Clear
              </Button>
            )}
          </>
        }
      />
      <Paper withBorder p="sm" radius="md">
        <Stack gap="xs">
          <Group gap="sm" wrap="wrap" align="flex-end">
            <SegmentedControl
              value={matched}
              onChange={(v) => setParam('matched', v)}
              data={[
                { label: 'All', value: 'all' },
                { label: 'Matched', value: 'matched' },
                { label: 'Unmatched', value: 'unmatched' },
              ]}
              aria-label="Matched filter"
            />
            <Select placeholder="Any method" data={METHODS} value={method} onChange={(v) => setParam('method', v)} clearable w={140} aria-label="Method" />
            <TextInput
              placeholder="Path prefix"
              value={pathPrefix}
              onChange={(e) => {
                setPathPrefix(e.currentTarget.value);
                setParam('pathPrefix', e.currentTarget.value);
              }}
              aria-label="Path prefix"
              style={{ flex: '1 1 180px' }}
              className="mono"
            />
            <TextInput
              placeholder="Source (stubs, plugin)"
              value={source}
              onChange={(e) => {
                setSource(e.currentTarget.value);
                setParam('source', e.currentTarget.value);
              }}
              aria-label="Source"
              w={170}
            />
          </Group>
          <Group gap="sm" wrap="wrap" align="flex-end">
            <TextInput
              placeholder="Body contains"
              value={bodyContains}
              onChange={(e) => {
                setBodyContains(e.currentTarget.value);
                setParam('bodyContains', e.currentTarget.value);
              }}
              aria-label="Body contains"
              style={{ flex: '1 1 180px' }}
            />
            <TextInput
              placeholder="Correlation"
              value={correlation}
              onChange={(e) => {
                setCorrelation(e.currentTarget.value);
                setParam('correlation', e.currentTarget.value);
              }}
              aria-label="Correlation"
              w={180}
            />
            <TextInput type="datetime-local" value={since} onChange={(e) => setParam('since', e.currentTarget.value)} aria-label="Since" w={210} />
            <NumberInput value={limit} onChange={(v) => setParam('limit', String(v || 200))} min={1} max={10000} w={100} aria-label="Limit" />
          </Group>
        </Stack>
      </Paper>

      {list.isLoading ? (
        <LoadingState />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : !list.data?.requests.length ? (
        <EmptyState title="No requests yet">Requests to the mock port appear here {live ? 'within two seconds' : 'when you resume live updates'}.</EmptyState>
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table highlightOnHover fz="sm" verticalSpacing={6} miw={820} data-testid="journal-table">
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>When</Table.Th>
                  <Table.Th>Method</Table.Th>
                  <Table.Th>Path</Table.Th>
                  <Table.Th>Status</Table.Th>
                  <Table.Th>Match</Table.Th>
                  <Table.Th>Time</Table.Th>
                  <Table.Th>Source</Table.Th>
                  {spaces.enabled && <Table.Th>Space</Table.Th>}
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {list.data.requests.map((r) => (
                  <Table.Tr
                    key={r.id}
                    className="clickable-row"
                    tabIndex={0}
                    aria-label={`${r.method} ${r.path} ${r.status}`}
                    onClick={() => setParam('id', String(r.id))}
                    onKeyDown={(e) => {
                      if (e.key === 'Enter' || e.key === ' ') {
                        e.preventDefault();
                        setParam('id', String(r.id));
                      }
                    }}
                    data-testid="journal-row"
                  >
                    <Table.Td>
                      <Time at={r.at} />
                    </Table.Td>
                    <Table.Td>
                      <MethodBadge method={r.method} />
                    </Table.Td>
                    <Table.Td className="mono" maw={420} style={{ wordBreak: 'break-all' }}>
                      {r.path}
                      {r.query ? <Text span c="dimmed" inherit>{`?${r.query}`}</Text> : null}
                    </Table.Td>
                    <Table.Td>
                      <StatusBadge status={r.status} />
                    </Table.Td>
                    <Table.Td>
                      <Badge size="sm" color={r.matched ? 'green' : 'orange'} variant="dot">
                        {r.matched ? 'matched' : 'unmatched'}
                      </Badge>
                    </Table.Td>
                    <Table.Td>{r.durationMs} ms</Table.Td>
                    <Table.Td>{r.source}</Table.Td>
                    {spaces.enabled && (
                      <Table.Td>
                        <SpaceBadge space={r.space} />
                      </Table.Td>
                    )}
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
          <Text size="xs" c="dimmed" p="sm">
            Showing {list.data.requests.length} entries{live ? ' · live' : ' · paused'}
          </Text>
        </Paper>
      )}
      <Drawer opened={!!selectedId} onClose={() => setParam('id', null)} position="right" size="xl" title="Request detail">
        {selected ? <EntryDetail entry={selected} /> : single.isError ? <ErrorState error={single.error} /> : <LoadingState />}
      </Drawer>
    </Stack>
  );
}
