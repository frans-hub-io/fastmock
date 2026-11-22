import {
  ActionIcon,
  Anchor,
  Badge,
  Button,
  Code,
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
import { IconActivity, IconPlayerPause, IconPlayerPlay, IconPlus, IconRefresh, IconSearch, IconTrash } from '@tabler/icons-react';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { JournalEntry } from '../api/types';
import { useCan, useSpace, useSpaceQuery } from '../auth/useAuth';
import { SpaceBadge } from '../components/spaces';
import { BodyView, Copy, DataTable, DensityToggle, KeyValueTable, MethodBadge, PageHeader, PathText, StatusBadge, Time, ToneBadge } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { GrpcCallView } from '../components/GrpcView';
import { grpcCodeName } from '../lib/grpc';
import { toCurl } from '../lib/curl';
import { formatAbsolute, headerValue } from '../lib/format';
import { docsUrl } from '../lib/docs';
import { stubFromJournal } from '../lib/stubs';
import { formatMs } from '../lib/traffic';
import { useDensity } from '../lib/ui';
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
      <Paper withBorder radius="md" p="sm" className="fm-detail-head">
        <Group gap="sm" wrap="nowrap" align="flex-start">
          <MethodBadge method={entry.method} />
          <Text component="div" size="md" fw={600} className="fm-grow">
            <PathText path={entry.path} query={entry.query} />
          </Text>
        </Group>
        <Group gap="xs" wrap="wrap" mt="sm">
          <StatusBadge status={entry.status} />
          <ToneBadge tone={entry.matched ? 'success' : 'warning'}>{entry.matched ? 'matched' : 'unmatched'}</ToneBadge>
          <Badge variant="outline" color="gray" tt="none">
            {entry.durationMs} ms
          </Badge>
          <Badge variant="outline" color="gray" tt="none">
            {entry.source}
          </Badge>
          {entry.grpc && (
            <ToneBadge tone={entry.grpc.status === 0 ? 'success' : 'danger'} testId="grpc-status-head">
              gRPC {grpcCodeName(entry.grpc.status)}
            </ToneBadge>
          )}
        </Group>
      </Paper>
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
              {entry.grpc ? 'Messages' : 'Body'}
            </Text>
            {entry.grpc ? <GrpcCallView entry={entry} side="request" /> : <BodyView body={entry.body} contentType={headerValue(entry.headers, 'content-type')} />}
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
              {entry.grpc ? 'Messages' : 'Body'}
            </Text>
            {entry.grpc ? <GrpcCallView entry={entry} side="response" /> : <BodyView body={entry.responseBody} contentType={undefined} />}
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
  const [density, setDensity] = useDensity();
  const [more, setMore] = useState(false);
  const can = useCan();
  const canStub = can('stubs:write');
  const navigate = useNavigate();
  const cfg = useUiConfig();
  const dataBase = storedDataPlaneBase() ?? defaultDataPlaneBase(cfg.data);
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
        meta={
          <span className="fm-live" data-live={live || undefined} data-testid="live-indicator">
            <span className="fm-live-dot" aria-hidden />
            {live ? 'Live, refreshing every 2 seconds' : 'Paused'}
          </span>
        }
        actions={
          <>
            {canEdit && (
              <Button
                variant="subtle"
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
            <Tooltip label="Refresh now">
              <ActionIcon variant="default" size="lg" onClick={() => void list.refetch()} aria-label="Refresh">
                <IconRefresh size={18} />
              </ActionIcon>
            </Tooltip>
            <Button
              variant={live ? 'light' : 'filled'}
              leftSection={live ? <IconPlayerPause size={16} /> : <IconPlayerPlay size={16} />}
              onClick={() => setLive((v) => !v)}
              data-testid="live-toggle"
            >
              {live ? 'Pause' : 'Resume'}
            </Button>
          </>
        }
      />
      <div className="fm-toolbar">
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
          leftSection={<IconSearch size={16} />}
          value={pathPrefix}
          onChange={(e) => {
            setPathPrefix(e.currentTarget.value);
            setParam('pathPrefix', e.currentTarget.value);
          }}
          aria-label="Path prefix"
          className="fm-grow"
          classNames={{ input: 'mono' }}
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
        <DensityToggle value={density} onChange={setDensity} />
        <Button variant="default" className="fm-more-toggle" onClick={() => setMore((m) => !m)} aria-expanded={more}>
          {more ? 'Fewer filters' : 'More filters'}
        </Button>
        <div className="fm-toolbar-break" />
        <TextInput
          placeholder="Body contains"
          value={bodyContains}
          onChange={(e) => {
            setBodyContains(e.currentTarget.value);
            setParam('bodyContains', e.currentTarget.value);
          }}
          aria-label="Body contains"
          className="fm-grow fm-more"
          data-open={more || undefined}
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
          className="fm-more"
          data-open={more || undefined}
        />
        <TextInput type="datetime-local" value={since} onChange={(e) => setParam('since', e.currentTarget.value)} aria-label="Since" w={210} className="fm-more" data-open={more || undefined} />
        <NumberInput value={limit} onChange={(v) => setParam('limit', String(v || 200))} min={1} max={10000} w={100} aria-label="Limit" className="fm-more" data-open={more || undefined} />
      </div>

      {list.isLoading ? (
        <LoadingState />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : !list.data?.requests.length ? (
        <EmptyState
          title={params.toString() && params.toString() !== `id=${selectedId ?? ''}` ? 'No requests match the filters' : 'No requests yet'}
          icon={<IconActivity size={30} />}
          docs={{ label: 'Getting started with FastMock', href: docsUrl('guide/getting-started') }}
        >
          <Stack gap="xs" align="center">
            <span>Requests to the mock port appear here {live ? 'within two seconds' : 'when you resume live updates'}. Try one:</span>
            <Group gap={4} wrap="nowrap" className="fm-cmd">
              <Code className="fm-cmd-code">{`curl -i ${dataBase.replace(/\/+$/, '')}/hello`}</Code>
              <Copy value={`curl -i ${dataBase.replace(/\/+$/, '')}/hello`} label="Copy command" />
            </Group>
          </Stack>
        </EmptyState>
      ) : (
        <DataTable
          testId="journal-table"
          miw={880}
          density={density}
          footer={
            <Text size="xs" c="dimmed">
              Showing {list.data.requests.length} entries{live ? ' · live' : ' · paused'}
            </Text>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th>When</Table.Th>
              <Table.Th>Request</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th>Match</Table.Th>
              <Table.Th>Duration</Table.Th>
              <Table.Th>Source</Table.Th>
              {spaces.enabled && <Table.Th>Space</Table.Th>}
              <Table.Th w={84}>
                <span className="sr-only">Actions</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {list.data.requests.map((r) => (
              <Table.Tr
                key={r.id}
                className={`clickable-row ${String(r.id) === selectedId ? 'fm-row-selected' : ''}`}
                tabIndex={0}
                aria-label={`${r.method} ${r.path} ${String(r.status)}`}
                onClick={() => setParam('id', String(r.id))}
                onKeyDown={(e) => {
                  if (e.target !== e.currentTarget) return;
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    setParam('id', String(r.id));
                  }
                }}
                data-testid="journal-row"
              >
                <Table.Td data-label="When">
                  <Time at={r.at} />
                </Table.Td>
                <Table.Td>
                  <Group gap={8} wrap="nowrap">
                    <MethodBadge method={r.method} />
                    <PathText path={r.path} query={r.query} />
                  </Group>
                </Table.Td>
                <Table.Td data-label="Status">
                  <StatusBadge status={r.status} />
                </Table.Td>
                <Table.Td data-label="Match">
                  <ToneBadge tone={r.matched ? 'success' : 'warning'}>{r.matched ? 'matched' : 'unmatched'}</ToneBadge>
                </Table.Td>
                <Table.Td data-label="Duration" className="tabular">
                  {formatMs(r.durationMs)}
                </Table.Td>
                <Table.Td data-label="Source">
                  <span className="fm-chip">{r.source}</span>
                </Table.Td>
                {spaces.enabled && (
                  <Table.Td data-label="Space">
                    <SpaceBadge space={r.space} />
                  </Table.Td>
                )}
                <Table.Td data-empty={!canStub && r.matched ? true : undefined}>
                  <Group gap={2} wrap="nowrap" justify="flex-end" className="fm-row-actions">
                    {!r.matched && (
                      <Tooltip label="Explain with near misses">
                        <ActionIcon
                          variant="subtle"
                          color="gray"
                          aria-label={`Explain ${r.method} ${r.path}`}
                          onClick={(e) => {
                            e.stopPropagation();
                            void navigate(`/near-misses?id=${String(r.id)}`);
                          }}
                        >
                          <IconSearch size={16} />
                        </ActionIcon>
                      </Tooltip>
                    )}
                    {canStub && (
                      <Tooltip label="Create stub from this request">
                        <ActionIcon
                          variant="subtle"
                          color="gray"
                          aria-label={`Create stub from ${r.method} ${r.path}`}
                          onClick={(e) => {
                            e.stopPropagation();
                            void navigate('/stubs/new', { state: { stub: { ...stubFromJournal(r), ...(r.space ? { space: r.space } : {}) } } });
                          }}
                        >
                          <IconPlus size={16} />
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
      <Drawer opened={!!selectedId} onClose={() => setParam('id', null)} position="right" size="xl" title="Request detail">
        {selected ? <EntryDetail entry={selected} /> : single.isError ? <ErrorState error={single.error} /> : <LoadingState />}
      </Drawer>
    </Stack>
  );
}
