import { ActionIcon, Alert, Button, Collapse, Group, Select, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { IconAlertTriangle, IconFilter, IconInbox, IconRefresh, IconSearch, IconTrash, IconX } from '@tabler/icons-react';
import { useInfiniteQuery, useQuery } from '@tanstack/react-query';
import { useEffect, useMemo, useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { captureApi, type MessageSummary } from '../../api/capture';
import { confirmAction, useApiMutation } from '../../api/mutations';
import { useCan, useSpace, useSpaceQuery } from '../../auth/useAuth';
import { CodeEditor } from '../../components/CodeEditor';
import { DataTable, Time } from '../../components/common';
import { SpaceBadge } from '../../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { destinationOf, parseMatch, PROTOCOLS, RANGES, sinceOf, withFilters } from '../../lib/capture';
import { formatBytes } from '../../lib/format';
import { ProtocolBadge } from './parts';

const PAGE = 50;
const FIND_LIMIT = 200;
const MATCH_EXAMPLE = '{\n  "headers": { "traceparent": { "contains": "4bf92f35" } },\n  "body": [{ "jsonPath": "$.status", "value": { "equalTo": "PAID" } }]\n}';

function MessageRow({ m, showSpace }: { m: MessageSummary; showSpace: boolean }) {
  const navigate = useNavigate();
  const open = () => void navigate(`/messages/${m.id}`);
  return (
    <Table.Tr
      className="clickable-row"
      tabIndex={0}
      aria-label={`${m.source}: ${destinationOf(m)}`}
      onClick={open}
      onKeyDown={(e) => {
        if (e.target !== e.currentTarget) return;
        if (e.key === 'Enter' || e.key === ' ') {
          e.preventDefault();
          open();
        }
      }}
      data-testid="message-row"
    >
      <Table.Td data-label="Received">
        <Time at={m.receivedAt} />
      </Table.Td>
      <Table.Td data-label="Source">
        <Text size="sm" className="break-all">
          {m.source}
        </Text>
      </Table.Td>
      <Table.Td data-label="Protocol">
        <ProtocolBadge protocol={m.protocol} />
      </Table.Td>
      <Table.Td data-label={m.protocol === 'amqp' ? 'Exchange → key' : 'Topic'}>
        <Link to={`/messages/${m.id}`} className="fm-path break-all" onClick={(e) => e.stopPropagation()} data-testid="message-destination">
          {destinationOf(m)}
        </Link>
      </Table.Td>
      <Table.Td data-label="Key">
        <Text size="sm" className="mono break-all" c={m.key ? undefined : 'dimmed'}>
          {m.key ?? '—'}
        </Text>
      </Table.Td>
      <Table.Td data-label="Preview" maw={360}>
        <Text size="sm" c="dimmed" className="mono truncate" title={m.preview ?? undefined}>
          {m.preview ?? ''}
        </Text>
      </Table.Td>
      <Table.Td data-label="Size" ta="right" className="tabular">
        {formatBytes(m.size)}
        {m.truncated && (
          <Text span size="xs" c="dimmed">
            {' '}
            (truncated)
          </Text>
        )}
      </Table.Td>
      {showSpace && (
        <Table.Td data-label="Space">
          <SpaceBadge space={m.space} />
        </Table.Td>
      )}
    </Table.Tr>
  );
}

export function MessageList() {
  const can = useCan();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [params, setParams] = useSearchParams();
  const source = params.get('source') ?? '';
  const protocol = params.get('protocol') ?? '';
  const topic = params.get('topic') ?? '';
  const range = params.get('range') ?? 'any';
  const matchText = params.get('match') ?? '';
  const [topicText, setTopicText] = useState(topic);
  const [matchOpen, setMatchOpen] = useState(!!matchText);
  const [draftMatch, setDraftMatch] = useState(matchText || MATCH_EXAMPLE);
  const [anchor, setAnchor] = useState(() => Date.now());

  useEffect(() => setTopicText(topic), [topic]);

  const setParam = (k: string, v: string | null) => {
    const next = new URLSearchParams(params);
    if (!v) next.delete(k);
    else next.set(k, v);
    setParams(next, { replace: true });
  };

  const since = sinceOf(range, anchor);
  const applied = parseMatch(matchText);
  const finding = !!applied.match;
  const draftProblem = matchOpen ? parseMatch(draftMatch).error : undefined;

  const sources = useQuery({ queryKey: ['capture', 'sources', spaceQ.space ?? null], queryFn: () => captureApi.sources(spaceQ.space) });
  const sourceNames = useMemo(() => [...new Set((sources.data?.sources ?? []).map((s) => s.name))].sort(), [sources.data]);

  const filters = { source: source || undefined, protocol: protocol || undefined, topic: topic || undefined };
  const list = useInfiniteQuery({
    queryKey: ['capture', 'list', spaceQ.space ?? null, filters, since ?? null],
    queryFn: ({ pageParam }) => captureApi.list({ ...spaceQ, ...filters, since, cursor: pageParam, limit: PAGE }),
    initialPageParam: undefined as string | undefined,
    getNextPageParam: (last) => last.nextCursor ?? undefined,
    enabled: !finding,
  });
  const found = useQuery({
    queryKey: ['capture', 'find', spaceQ.space ?? null, filters, since ?? null, matchText],
    queryFn: () => captureApi.find({ match: withFilters(applied.match ?? {}, filters), since, limit: FIND_LIMIT }, spaceQ.space),
    enabled: finding,
  });

  const items = useMemo(() => {
    if (finding) return found.data?.messages ?? [];
    const seen = new Set<string>();
    const out: MessageSummary[] = [];
    for (const m of list.data?.pages.flatMap((p) => p.messages) ?? []) {
      if (seen.has(m.id)) continue;
      seen.add(m.id);
      out.push(m);
    }
    return out;
  }, [finding, found.data, list.data]);

  const active = finding ? found : list;
  const reload = () => {
    setAnchor(Date.now());
    void active.refetch();
  };
  const filtered = !!(source || protocol || topic || range !== 'any' || finding);
  const showSpace = spaces.enabled && !spaces.current;
  const canDelete = can('messages:delete');
  const where = spaces.current ? ` in space ${spaces.current}` : spaces.enabled ? ' in every space you may delete in' : '';

  const clear = useApiMutation<void, { deleted: number }>({
    fn: () => captureApi.removeAll({ ...spaceQ, source: source || undefined }),
    invalidate: [['capture']],
    success: (d) => `Deleted ${String(d.deleted)} message${d.deleted === 1 ? '' : 's'}`,
  });

  const applyMatch = () => {
    if (parseMatch(draftMatch).error) return;
    setParam('match', draftMatch.trim() || null);
  };

  return (
    <Stack gap="md">
      <div className="fm-toolbar">
        <Select
          placeholder="All sources"
          aria-label="Source"
          data={sourceNames.includes(source) || !source ? sourceNames : [source, ...sourceNames]}
          value={source || null}
          onChange={(v) => setParam('source', v)}
          clearable
          searchable
          w={{ base: '100%', sm: 200 }}
          data-testid="filter-source"
        />
        <Select
          placeholder="All protocols"
          aria-label="Protocol"
          data={PROTOCOLS}
          value={protocol || null}
          onChange={(v) => setParam('protocol', v)}
          clearable
          w={{ base: '100%', sm: 150 }}
          data-testid="filter-protocol"
        />
        <form
          className="fm-grow"
          role="search"
          onSubmit={(e) => {
            e.preventDefault();
            setParam('topic', topicText.trim() || null);
          }}
        >
          <TextInput
            placeholder="Exact topic or routing key"
            aria-label="Topic"
            leftSection={<IconSearch size={16} />}
            value={topicText}
            onChange={(e) => setTopicText(e.currentTarget.value)}
            onBlur={() => topicText.trim() !== topic && setParam('topic', topicText.trim() || null)}
            rightSection={
              topicText ? (
                <ActionIcon variant="subtle" color="gray" aria-label="Clear topic" onClick={() => setParam('topic', null)}>
                  <IconX size={16} />
                </ActionIcon>
              ) : null
            }
            classNames={{ input: 'mono' }}
            data-testid="filter-topic"
          />
        </form>
        <Select
          aria-label="Time range"
          data={RANGES.map((r) => ({ value: r.value, label: r.label }))}
          value={range}
          onChange={(v) => {
            setAnchor(Date.now());
            setParam('range', v && v !== 'any' ? v : null);
          }}
          allowDeselect={false}
          w={{ base: '100%', sm: 170 }}
          data-testid="filter-range"
        />
        <Group gap="xs" wrap="nowrap">
          <Tooltip label="Match keys, headers, properties and bodies">
            <Button variant={finding ? 'light' : 'default'} leftSection={<IconFilter size={16} />} onClick={() => setMatchOpen((o) => !o)} data-testid="match-toggle" aria-expanded={matchOpen}>
              Match
            </Button>
          </Tooltip>
          <Tooltip label="Reload">
            <ActionIcon variant="default" size="lg" onClick={reload} aria-label="Reload">
              <IconRefresh size={18} />
            </ActionIcon>
          </Tooltip>
          {canDelete && items.length > 0 && (
            <Button
              variant="subtle"
              color="red"
              leftSection={<IconTrash size={16} />}
              data-testid="messages-delete-all"
              onClick={() =>
                confirmAction({
                  title: source ? `Delete the messages of ${source}?` : 'Delete all messages?',
                  message: source ? `Every stored message of the source ${source}${where} is deleted. This cannot be undone.` : `Every captured message${where} is deleted. This cannot be undone.`,
                  confirm: 'Delete',
                  onConfirm: () => clear.mutate(),
                })
              }
            >
              {source ? 'Delete source messages' : 'Delete all'}
            </Button>
          )}
        </Group>
      </div>

      <Collapse expanded={matchOpen}>
        <Stack gap="xs" data-testid="match-panel">
          <Text size="xs" c="dimmed">
            A JSON match: <code>source</code>, <code>protocol</code>, <code>topic</code>, <code>exchange</code>, <code>key</code>, <code>contentType</code> take string matchers (<code>equalTo</code>, <code>contains</code>, <code>matches</code>…);{' '}
            <code>headers</code> and <code>properties</code> map names to matchers; <code>body</code> takes the body matchers of stubs. The filters above are added.
          </Text>
          <CodeEditor value={draftMatch} onChange={setDraftMatch} minHeight={120} maxHeight={320} label="Message match JSON" testId="match-editor" />
          {draftProblem && (
            <Alert color="red" icon={<IconAlertTriangle size={18} />} p="xs">
              {draftProblem}
            </Alert>
          )}
          <Group gap="xs" justify="flex-end">
            {finding && (
              <Button variant="subtle" color="gray" onClick={() => setParam('match', null)} data-testid="match-clear">
                Clear match
              </Button>
            )}
            <Button onClick={applyMatch} disabled={!!draftProblem} leftSection={<IconSearch size={16} />} data-testid="match-apply">
              Find
            </Button>
          </Group>
        </Stack>
      </Collapse>

      {active.isLoading ? (
        <LoadingState />
      ) : active.isError ? (
        <ErrorState error={active.error} onRetry={reload} />
      ) : !items.length ? (
        filtered ? (
          <EmptyState
            title="No messages match"
            icon={<IconSearch size={28} />}
            action={
              <Button variant="light" onClick={() => setParams(new URLSearchParams(), { replace: true })}>
                Clear filters
              </Button>
            }
          >
            Nothing matches the filters{spaces.current ? ` in space ${spaces.current}` : ''}.
          </EmptyState>
        ) : (
          <EmptyState
            title="No messages captured yet"
            icon={<IconInbox size={30} />}
            action={
              <Button component={Link} to="/messages/sources" variant="light">
                {can('messages:write') ? 'Add a source' : 'View sources'}
              </Button>
            }
            testId="messages-empty"
          >
            FastMock stores the messages of the topics, topic filters and exchanges its sources subscribe to. They appear here as they arrive.
          </EmptyState>
        )
      ) : (
        <DataTable
          testId="messages-table"
          miw={960}
          footer={
            <Group justify="space-between" w="100%" gap="sm">
              <Text size="xs" c="dimmed" data-testid="messages-count">
                {finding && found.data
                  ? `${String(found.data.count)} matching message${found.data.count === 1 ? '' : 's'}${found.data.count > items.length ? `, the newest ${String(items.length)} shown` : ''}`
                  : `${String(items.length)} message${items.length === 1 ? '' : 's'}, newest first`}
              </Text>
              {!finding && list.hasNextPage && (
                <Button size="compact-sm" variant="light" loading={list.isFetchingNextPage} onClick={() => void list.fetchNextPage()} data-testid="messages-load-more">
                  Load more
                </Button>
              )}
            </Group>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Received</Table.Th>
              <Table.Th>Source</Table.Th>
              <Table.Th>Protocol</Table.Th>
              <Table.Th>Topic</Table.Th>
              <Table.Th>Key</Table.Th>
              <Table.Th>Preview</Table.Th>
              <Table.Th ta="right">Size</Table.Th>
              {showSpace && <Table.Th>Space</Table.Th>}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {items.map((m) => (
              <MessageRow key={m.id} m={m} showSpace={showSpace} />
            ))}
          </Table.Tbody>
        </DataTable>
      )}
    </Stack>
  );
}
