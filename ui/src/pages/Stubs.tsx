import {
  ActionIcon,
  Badge,
  Button,
  Group,
  Menu,
  Pagination,
  Paper,
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
  IconPlus,
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
import { MethodBadge, PageHeader, StatusBadge, Time } from '../components/common';
import { ImportModal } from '../components/ImportModal';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { downloadText, timestampedName } from '../lib/download';
import { duplicateStub, stripServerFields, stubPath, stubTitle } from '../lib/stubs';

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
      <Paper withBorder p="sm" radius="md">
        <Group gap="sm" wrap="wrap">
          <TextInput
            placeholder="Search name or path"
            leftSection={<IconSearch size={16} />}
            value={search}
            onChange={(e) => {
              setSearch(e.currentTarget.value);
              setParam('q', e.currentTarget.value);
            }}
            aria-label="Search stubs"
            style={{ flex: '1 1 240px' }}
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
        </Group>
      </Paper>

      {list.isLoading ? (
        <LoadingState />
      ) : list.isError ? (
        <ErrorState error={list.error} onRetry={() => void list.refetch()} />
      ) : !list.data?.stubs.length ? (
        <EmptyState title={debounced || tag || enabled !== 'all' ? 'No stubs match the filters' : 'No stubs yet'}>
          {canEdit ? 'Create a stub, import a WireMock/OpenAPI/Postman/HAR file, or record traffic.' : 'Ask an editor to add stubs.'}
        </EmptyState>
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table highlightOnHover verticalSpacing="xs" fz="sm" miw={960} data-testid="stubs-table">
              <Table.Thead>
                <Table.Tr>
                  <Table.Th w={70}>On</Table.Th>
                  <Table.Th>Name</Table.Th>
                  <Table.Th>Method</Table.Th>
                  <Table.Th>Path</Table.Th>
                  <Table.Th>Status</Table.Th>
                  <Table.Th>Prio</Table.Th>
                  <Table.Th>Tags</Table.Th>
                  {spaces.enabled && <Table.Th>Space</Table.Th>}
                  <Table.Th>Updated</Table.Th>
                  <Table.Th w={50}>
                    <span className="sr-only">Actions</span>
                  </Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {list.data.stubs.map((s) => {
                  const editable = can('stubs:write', s.space ?? null);
                  return (
                    <Table.Tr key={s.id} data-testid="stub-row">
                      <Table.Td>
                        <Permitted perm="stubs:write" where={s.space ?? null}>
                          <Switch
                            checked={s.enabled !== false}
                            disabled={toggle.isPending}
                            onChange={(e) => toggle.mutate({ stub: s, enabled: e.currentTarget.checked })}
                            aria-label={`${s.enabled !== false ? 'Disable' : 'Enable'} ${stubTitle(s)}`}
                            size="sm"
                          />
                        </Permitted>
                      </Table.Td>
                      <Table.Td maw={280}>
                        <Text component={Link} to={`/stubs/${s.id ?? ''}`} size="sm" fw={500} c="var(--mantine-color-anchor)" className="truncate" display="block">
                          {stubTitle(s)}
                        </Text>
                      </Table.Td>
                      <Table.Td>
                        <MethodBadge method={s.request.method} />
                      </Table.Td>
                      <Table.Td className="mono" maw={320} style={{ wordBreak: 'break-all' }}>
                        {stubPath(s)}
                      </Table.Td>
                      <Table.Td>
                        <StatusBadge status={s.response?.status ?? 200} />
                      </Table.Td>
                      <Table.Td>{s.priority ?? 5}</Table.Td>
                      <Table.Td maw={220}>
                        <Group gap={4}>
                          {(s.tags ?? []).slice(0, 4).map((t) => (
                            <Badge
                              key={t}
                              size="sm"
                              variant="dot"
                              style={{ cursor: 'pointer', textTransform: 'none' }}
                              onClick={() => setParam('tag', t)}
                            >
                              {t}
                            </Badge>
                          ))}
                          {(s.tags?.length ?? 0) > 4 && <Text size="xs">+{(s.tags?.length ?? 0) - 4}</Text>}
                        </Group>
                      </Table.Td>
                      {spaces.enabled && (
                        <Table.Td>
                          <SpaceBadge space={s.space} />
                        </Table.Td>
                      )}
                      <Table.Td>
                        <Time at={s.updatedAt} />
                      </Table.Td>
                      <Table.Td>
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
                      </Table.Td>
                    </Table.Tr>
                  );
                })}
              </Table.Tbody>
            </Table>
          </div>
          <Group justify="space-between" p="sm">
            <Text size="sm" c="dimmed">
              {total} stub{total === 1 ? '' : 's'}
            </Text>
            {pages > 1 && <Pagination total={pages} value={page} onChange={(p) => setParam('page', String(p))} size="sm" />}
          </Group>
        </Paper>
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
