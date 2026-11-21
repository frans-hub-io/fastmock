import {
  ActionIcon,
  Button,
  Group,
  Menu,
  Pagination,
  SegmentedControl,
  Select,
  Stack,
  Switch,
  Table,
  Text,
  TextInput,
  Tooltip,
} from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import {
  IconCopy,
  IconDotsVertical,
  IconDownload,
  IconEdit,
  IconInfoCircle,
  IconPlus,
  IconRoute,
  IconSearch,
  IconTags,
  IconTrash,
  IconUpload,
} from '@tabler/icons-react';
import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link, useNavigate, useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { Stub } from '../api/types';
import { useAuth, useCan, useSpace, useSpaceQuery } from '../auth/useAuth';
import { Permitted } from '../components/Permitted';
import { SpaceBadge } from '../components/spaces';
import { DataTable, DensityToggle, MethodBadge, PageHeader, PathText, StatusBadge, Time } from '../components/common';
import { ImportModal } from '../components/ImportModal';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { downloadText, timestampedName } from '../lib/download';
import { docsUrl } from '../lib/docs';
import { pathKindOf } from '../lib/path';
import { duplicateStub, stripServerFields, stubPath, stubTitle, urlKind } from '../lib/stubs';
import { hitsByStub } from '../lib/traffic';
import { useDensity } from '../lib/ui';

const PAGE_SIZE = 25;

export default function Stubs() {
  const can = useCan();
  const canEdit = can('stubs:write');
  const canImport = can('imports:run');
  const { role } = useAuth();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const navigate = useNavigate();
  const [params, setParams] = useSearchParams();
  const [search, setSearch] = useState(params.get('q') ?? '');
  const [debounced] = useDebouncedValue(search, 300);
  const tag = params.get('tag');
  const enabled = params.get('enabled') ?? 'all';
  const page = Math.max(1, Number(params.get('page') ?? '1') || 1);
  const [importOpen, setImportOpen] = useState(false);
  const [density, setDensity] = useDensity();
  const readJournal = can('journal:read');

  const setParam = (k: string, v: string | null) => {
    const next = new URLSearchParams(params);
    if (v === null || v === '' || v === 'all') next.delete(k);
    else next.set(k, v);
    if (k !== 'page') next.delete('page');
    setParams(next, { replace: true });
  };

  const query = {
    q: debounced.trim() || undefined,
    tag: tag ?? undefined,
    enabled: enabled === 'all' ? undefined : enabled === 'enabled',
    limit: PAGE_SIZE,
    offset: (page - 1) * PAGE_SIZE,
    ...spaceQ,
  };
  const list = useQuery({
    queryKey: ['stubs', query],
    queryFn: () => unwrap(client.GET('/__admin/stubs', { params: { query } })),
    placeholderData: keepPreviousData,
  });
  const allTags = useQuery({
    queryKey: ['stubs', 'tags', spaceQ],
    queryFn: async () => {
      const all = await unwrap(client.GET('/__admin/stubs', { params: { query: { limit: 10000, ...spaceQ } } }));
      return [...new Set(all.stubs.flatMap((s) => s.tags ?? []))].sort();
    },
    staleTime: 30_000,
  });

  const hits = useQuery({
    queryKey: ['requests', 'hits', spaceQ],
    queryFn: async () => {
      const r = await unwrap(client.GET('/__admin/requests', { params: { query: { matched: true, limit: 1000, ...spaceQ } } }));
      return new Map(hitsByStub(r.requests).map((h) => [h.stubId, h]));
    },
    enabled: readJournal,
    refetchInterval: 10_000,
  });

  const toggle = useApiMutation<{ stub: Stub; enabled: boolean }, Stub>({
    fn: ({ stub, enabled: en }) =>
      unwrap(client.PATCH('/__admin/stubs/{id}', { params: { path: { id: stub.id ?? '' } }, body: { enabled: en } as never, headers: { 'Content-Type': 'application/merge-patch+json' } })),
    invalidate: [['stubs'], ['info']],
    success: (_d, v) => `${stubTitle(v.stub)} ${v.enabled ? 'enabled' : 'disabled'}`,
  });
  const remove = useApiMutation<Stub>({
    fn: (s) => unwrap(client.DELETE('/__admin/stubs/{id}', { params: { path: { id: s.id ?? '' } } })),
    invalidate: [['stubs'], ['info']],
    success: (_d, s) => `Deleted ${stubTitle(s)}`,
  });
  const dup = useApiMutation<Stub, Stub>({
    fn: (s) => unwrap(client.POST('/__admin/stubs', { body: duplicateStub(s) })),
    invalidate: [['stubs'], ['info']],
    success: (d) => `Created ${stubTitle(d)}`,
  });
  const deleteTag = useApiMutation<string, { deleted: number }>({
    fn: (t) => unwrap(client.DELETE('/__admin/stubs', { params: { query: { tag: t, ...spaceQ } } })),
    invalidate: [['stubs'], ['info']],
    success: (d, t) => `Deleted ${d.deleted} stubs tagged ${t}`,
  });

  const exportStubs = async () => {
    const data = await unwrap(client.GET('/__admin/export', { params: { query: { tag: tag ?? undefined, ...spaceQ } } }));
    const base = ['fastmock-stubs', spaces.current, tag].filter(Boolean).join('-');
    downloadText(timestampedName(base, 'json'), JSON.stringify(data, null, 2));
  };

  const total = list.data?.total ?? 0;
  const pages = Math.max(1, Math.ceil(total / PAGE_SIZE));

  return (
    <Stack gap="md">
      <PageHeader
        title="Stubs"
        description="Request patterns and the responses FastMock returns for them."
        actions={
          <>
            <Button variant="default" leftSection={<IconDownload size={16} />} onClick={() => void exportStubs()}>
              Export{tag ? ` "${tag}"` : ''}
            </Button>
            {canImport && (
              <Button variant="default" leftSection={<IconUpload size={16} />} onClick={() => setImportOpen(true)}>
                Import
              </Button>
            )}
            {canEdit && (
              <>
                {tag && (
                  <Button
                    variant="light"
                    color="red"
                    leftSection={<IconTags size={16} />}
                    onClick={() =>
                      confirmAction({
                        title: spaces.current ? `Delete all stubs tagged "${tag}" in space ${spaces.current}?` : `Delete all stubs tagged "${tag}"?`,
                        message: 'This removes every stub carrying the tag. It cannot be undone.',
                        confirm: 'Delete stubs',
                        onConfirm: () => deleteTag.mutate(tag),
                      })
                    }
                  >
                    Delete tag
                  </Button>
                )}
                <Button component={Link} to="/stubs/new" leftSection={<IconPlus size={16} />} data-testid="new-stub">
                  New stub
                </Button>
              </>
            )}
          </>
        }
      />
      <div className="fm-toolbar">
        <TextInput
          placeholder="Search name or path"
          leftSection={<IconSearch size={16} />}
          value={search}
          onChange={(e) => {
            setSearch(e.currentTarget.value);
            setParam('q', e.currentTarget.value);
          }}
          aria-label="Search stubs"
          className="fm-grow"
          data-testid="stub-search"
        />
        <Select
          placeholder="All tags"
          data={allTags.data ?? []}
          value={tag}
          onChange={(v) => setParam('tag', v)}
          clearable
          searchable
          aria-label="Filter by tag"
          w={200}
        />
        <SegmentedControl
          value={enabled}
          onChange={(v) => setParam('enabled', v)}
          data={[
            { label: 'All', value: 'all' },
            { label: 'Enabled', value: 'enabled' },
            { label: 'Disabled', value: 'disabled' },
          ]}
          aria-label="Filter by state"
        />
        <DensityToggle value={density} onChange={setDensity} />
      </div>

      {list.isLoading ? (
        <LoadingState />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : !list.data?.stubs.length ? (
        debounced || tag || enabled !== 'all' ? (
          <EmptyState
            title="No stubs match the filters"
            icon={<IconSearch size={28} />}
            action={
              <Button variant="default" onClick={() => {
                setSearch('');
                setParams(new URLSearchParams(), { replace: true });
              }}>
                Clear filters
              </Button>
            }
          >
            Try another name, path or tag.
          </EmptyState>
        ) : (
          <EmptyState
            title="No stubs yet"
            icon={<IconRoute size={30} />}
            docs={{ label: 'How stubs match requests', href: docsUrl('guide/request-matching') }}
            action={
              canEdit || canImport ? (
                <>
                  {canImport && (
                    <Button variant="default" leftSection={<IconUpload size={16} />} onClick={() => setImportOpen(true)}>
                      Import from OpenAPI, WireMock or Postman
                    </Button>
                  )}
                  {canEdit && (
                    <Button component={Link} to="/stubs/new" leftSection={<IconPlus size={16} />}>
                      Create your first stub
                    </Button>
                  )}
                </>
              ) : undefined
            }
          >
            {canEdit ? 'A stub pairs a request pattern with the response FastMock returns. Create one, import a file, or record real traffic.' : 'Stubs pair a request pattern with a response. Ask an editor to add some.'}
          </EmptyState>
        )
      ) : (
        <DataTable
          testId="stubs-table"
          miw={1000}
          density={density}
          footer={
            <>
              <Text size="sm" c="dimmed">
                {total} stub{total === 1 ? '' : 's'}
              </Text>
              {pages > 1 && <Pagination total={pages} value={page} onChange={(p) => setParam('page', String(p))} size="sm" />}
            </>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th w={64}>On</Table.Th>
              <Table.Th>Name</Table.Th>
              <Table.Th>Request</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th>Prio</Table.Th>
              {readJournal && (
                <Table.Th>
                  <Tooltip label="Matches among the latest 1,000 journal entries" multiline maw={240}>
                    <Group gap={4} wrap="nowrap" component="span" tabIndex={0}>
                      Hits <IconInfoCircle size={13} aria-hidden />
                    </Group>
                  </Tooltip>
                </Table.Th>
              )}
              {spaces.enabled && <Table.Th>Space</Table.Th>}
              <Table.Th>Updated</Table.Th>
              <Table.Th w={110}>
                <span className="sr-only">Actions</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {list.data.stubs.map((s) => {
              const editable = can('stubs:write', s.space ?? null);
              const h = hits.data?.get(s.id ?? '');
              const off = s.enabled === false;
              return (
                <Table.Tr key={s.id} data-testid="stub-row" data-disabled={off || undefined}>
                  <Table.Td data-label="Enabled">
                    <Permitted perm="stubs:write" where={s.space ?? null}>
                      <Switch
                        checked={!off}
                        disabled={toggle.isPending}
                        onChange={(e) => toggle.mutate({ stub: s, enabled: e.currentTarget.checked })}
                        aria-label={`${!off ? 'Disable' : 'Enable'} ${stubTitle(s)}`}
                        size="sm"
                      />
                    </Permitted>
                  </Table.Td>
                  <Table.Td maw={300}>
                    <Text component={Link} to={`/stubs/${s.id ?? ''}`} size="sm" fw={600} c={off ? 'dimmed' : 'var(--mantine-color-anchor)'} className="truncate" display="block">
                      {stubTitle(s)}
                    </Text>
                    {(s.tags?.length ?? 0) > 0 && (
                      <Group gap={4} mt={4} wrap="wrap">
                        {(s.tags ?? []).slice(0, 4).map((t) => (
                          <button key={t} type="button" className="fm-chip" onClick={() => setParam('tag', t)} aria-label={`Filter by tag ${t}`}>
                            {t}
                          </button>
                        ))}
                        {(s.tags?.length ?? 0) > 4 && <Text size="xs" c="dimmed">+{(s.tags?.length ?? 0) - 4}</Text>}
                      </Group>
                    )}
                  </Table.Td>
                  <Table.Td>
                    <Group gap={8} wrap="nowrap" align="center">
                      <MethodBadge method={s.request.method} />
                      <PathText path={stubPath(s)} kind={pathKindOf(urlKind(s.request))} />
                    </Group>
                  </Table.Td>
                  <Table.Td data-label="Status">
                    <StatusBadge status={s.response?.status ?? 200} />
                  </Table.Td>
                  <Table.Td data-label="Priority">{s.priority ?? 5}</Table.Td>
                  {readJournal && (
                    <Table.Td data-label="Hits">
                      {h ? (
                        <Group gap={6} wrap="nowrap">
                          <Text size="sm" fw={700} className="tabular">
                            {h.hits}
                          </Text>
                          <Text size="xs" c="dimmed" component="span">
                            <Time at={h.lastAt} />
                          </Text>
                        </Group>
                      ) : (
                        <Text size="sm" c="dimmed">
                          0
                        </Text>
                      )}
                    </Table.Td>
                  )}
                  {spaces.enabled && (
                    <Table.Td data-label="Space">
                      <SpaceBadge space={s.space} />
                    </Table.Td>
                  )}
                  <Table.Td data-label="Updated">
                    <Time at={s.updatedAt} />
                  </Table.Td>
                  <Table.Td>
                    <Group gap={2} wrap="nowrap" justify="flex-end">
                      <span className="fm-row-actions">
                        <Tooltip label={editable ? 'Edit' : 'View'}>
                          <ActionIcon variant="subtle" color="gray" onClick={() => void navigate(`/stubs/${s.id ?? ''}`)} aria-label={`${editable ? 'Edit' : 'View'} ${stubTitle(s)}`}>
                            <IconEdit size={16} />
                          </ActionIcon>
                        </Tooltip>
                        {editable && (
                          <Tooltip label="Duplicate">
                            <ActionIcon variant="subtle" color="gray" onClick={() => dup.mutate(s)} aria-label={`Duplicate ${stubTitle(s)}`}>
                              <IconCopy size={16} />
                            </ActionIcon>
                          </Tooltip>
                        )}
                      </span>
                      <Menu position="bottom-end" withinPortal>
                        <Menu.Target>
                          <ActionIcon variant="subtle" color="gray" aria-label={`Actions for ${stubTitle(s)}`}>
                            <IconDotsVertical size={16} />
                          </ActionIcon>
                        </Menu.Target>
                        <Menu.Dropdown>
                          <Menu.Item leftSection={<IconEdit size={16} />} onClick={() => void navigate(`/stubs/${s.id ?? ''}`)}>
                            {editable ? 'Edit' : 'View'}
                          </Menu.Item>
                          {editable && (
                            <Menu.Item leftSection={<IconCopy size={16} />} onClick={() => dup.mutate(s)}>
                              Duplicate
                            </Menu.Item>
                          )}
                          <Menu.Item
                            leftSection={<IconDownload size={16} />}
                            onClick={() => downloadText(`stub-${s.id ?? 'new'}.json`, JSON.stringify(stripServerFields(s), null, 2))}
                          >
                            Export JSON
                          </Menu.Item>
                          {editable && (
                            <>
                              <Menu.Divider />
                              <Menu.Item
                                color="red"
                                leftSection={<IconTrash size={16} />}
                                onClick={() =>
                                  confirmAction({
                                    title: 'Delete stub?',
                                    message: `"${stubTitle(s)}" will be removed. This cannot be undone.`,
                                    confirm: 'Delete',
                                    onConfirm: () => remove.mutate(s),
                                  })
                                }
                              >
                                Delete
                              </Menu.Item>
                            </>
                          )}
                        </Menu.Dropdown>
                      </Menu>
                    </Group>
                  </Table.Td>
                </Table.Tr>
              );
            })}
          </Table.Tbody>
        </DataTable>
      )}
      {!canEdit && (
        <Tooltip label={can.missing('stubs:write')}>
          <Text size="xs" c="dimmed" data-testid="read-only-note">
            {role === 'viewer' && !spaces.enabled ? 'Read-only view (viewer role).' : 'Read-only view.'}
          </Text>
        </Tooltip>
      )}
      <ImportModal opened={importOpen} onClose={() => setImportOpen(false)} />
    </Stack>
  );
}
