import { Anchor, Badge, Button, Card, Group, Paper, ScrollArea, Stack, Table, Text, TextInput, Title } from '@mantine/core';
import { IconExternalLink, IconSearch } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useMemo, useState } from 'react';
import { Link, useNavigate } from 'react-router';
import { client, unwrap } from '../api/client';
import { useAuth } from '../auth/useAuth';
import { MethodBadge, PageHeader, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { formatDuration } from '../lib/format';
import classes from './Dashboard.module.css';

function Stat({ label, value, to, testId }: { label: string; value: string | number | undefined; to?: string; testId?: string }) {
  const content = (
    <Paper withBorder p="md" radius="md" className={classes.stat}>
      <Text size="xs" c="dimmed" tt="uppercase" fw={700}>
        {label}
      </Text>
      <Text size="xl" fw={700} mt={4} data-testid={testId}>
        {value ?? '—'}
      </Text>
    </Paper>
  );
  return to ? (
    <Link to={to} className={classes.statLink}>
      {content}
    </Link>
  ) : (
    content
  );
}

function MetricsCard() {
  const [filter, setFilter] = useState('fastmock_');
  const q = useQuery({
    queryKey: ['metrics'],
    queryFn: async () => {
      const res = await fetch('/metrics', { credentials: 'omit', cache: 'no-store' });
      if (!res.ok) throw new Error(`metrics: ${res.status}`);
      return res.text();
    },
    refetchInterval: 15_000,
  });
  const lines = useMemo(() => {
    const f = filter.trim().toLowerCase();
    return (q.data ?? '')
      .split('\n')
      .filter((l) => l && !l.startsWith('#'))
      .filter((l) => !f || l.toLowerCase().includes(f))
      .slice(0, 400);
  }, [q.data, filter]);
  return (
    <Card withBorder radius="md">
      <Group justify="space-between" mb="sm">
        <Title order={3} size="h5">
          Prometheus metrics
        </Title>
        <Anchor href="/metrics" target="_blank" rel="noopener noreferrer" size="sm">
          <Group gap={4}>
            raw /metrics <IconExternalLink size={14} />
          </Group>
        </Anchor>
      </Group>
      <TextInput
        size="xs"
        mb="xs"
        leftSection={<IconSearch size={14} />}
        value={filter}
        onChange={(e) => setFilter(e.currentTarget.value)}
        aria-label="Filter metrics"
        placeholder="filter, e.g. fastmock_requests_total"
      />
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : (
        <ScrollArea h={260}>
          <pre className="pre" style={{ maxHeight: 'none', border: 0 }}>
            {lines.join('\n') || 'no matching series'}
          </pre>
        </ScrollArea>
      )}
    </Card>
  );
}

export default function Dashboard() {
  const navigate = useNavigate();
  const { refresh } = useAuth();
  const info = useQuery({ queryKey: ['info'], queryFn: () => unwrap(client.GET('/__admin/info')), refetchInterval: 10_000 });
  const version = useQuery({ queryKey: ['version'], queryFn: () => unwrap(client.GET('/__admin/version')), retry: false });
  const total = useQuery({ queryKey: ['requests', 'count', 'all'], queryFn: () => unwrap(client.GET('/__admin/requests/count')), refetchInterval: 5_000 });
  const unmatchedCount = useQuery({
    queryKey: ['requests', 'count', 'unmatched'],
    queryFn: () => unwrap(client.GET('/__admin/requests/count', { params: { query: { matched: false } } })),
    refetchInterval: 5_000,
  });
  const pending = useQuery({
    queryKey: ['callbacks', 'pending-count'],
    queryFn: () => unwrap(client.GET('/__admin/callbacks', { params: { query: { status: 'pending', limit: 1000 } } })),
    refetchInterval: 5_000,
  });
  const recent = useQuery({
    queryKey: ['requests', 'recent-unmatched'],
    queryFn: () => unwrap(client.GET('/__admin/requests', { params: { query: { matched: false, limit: 10 } } })),
    refetchInterval: 5_000,
  });

  if (info.isLoading) return <LoadingState />;
  if (info.isError) return <ErrorState error={info.error} onRetry={() => void info.refetch()} />;
  const i = info.data;
  const pendingCount = pending.data?.callbacks.length;
  const versionText = version.data?.version ?? i?.version;
  const extraVersion = version.data ? Object.entries(version.data).filter(([k, v]) => k !== 'version' && typeof v !== 'object') : [];

  return (
    <Stack gap="lg">
      <PageHeader
        title="Dashboard"
        description={`${i?.name ?? 'FastMock'} ${versionText ? `v${versionText}` : ''} · storage ${i?.storage ?? '?'} · up ${formatDuration(i?.uptimeSeconds ?? 0)}`}
        actions={
          <Button variant="default" size="xs" onClick={() => void Promise.all([info.refetch(), refresh()])}>
            Refresh
          </Button>
        }
      />
      <div className={classes.stats}>
        <Stat label="Stubs loaded" value={i?.stubsLoaded} to="/stubs" testId="stat-stubs" />
        <Stat label="Journal entries" value={total.data?.count} to="/requests" testId="stat-journal" />
        <Stat label="Unmatched" value={unmatchedCount.data?.count} to="/requests?matched=false" testId="stat-unmatched" />
        <Stat
          label="Pending callbacks"
          value={pendingCount === undefined ? undefined : pendingCount >= 1000 ? '1000+' : pendingCount}
          to="/callbacks?status=pending"
          testId="stat-callbacks"
        />
      </div>
      <div className={classes.grid}>
        <Card withBorder radius="md">
          <Group justify="space-between" mb="sm">
            <Title order={3} size="h5">
              Recent unmatched requests
            </Title>
            <Anchor component={Link} to="/near-misses" size="sm">
              Explain with near misses
            </Anchor>
          </Group>
          {recent.isLoading ? (
            <LoadingState />
          ) : recent.isError ? (
            <ErrorState error={recent.error} />
          ) : !recent.data?.requests.length ? (
            <EmptyState title="Every request matched a stub">Unmatched requests show up here as they arrive.</EmptyState>
          ) : (
            <div className="table-scroll">
              <Table highlightOnHover fz="sm" miw={520}>
                <Table.Thead>
                  <Table.Tr>
                    <Table.Th>When</Table.Th>
                    <Table.Th>Method</Table.Th>
                    <Table.Th>Path</Table.Th>
                    <Table.Th>Status</Table.Th>
                  </Table.Tr>
                </Table.Thead>
                <Table.Tbody>
                  {recent.data.requests.map((r) => (
                    <Table.Tr
                      key={r.id}
                      className="clickable-row"
                      tabIndex={0}
                      onClick={() => void navigate(`/requests?id=${r.id}`)}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') void navigate(`/requests?id=${r.id}`);
                      }}
                    >
                      <Table.Td>
                        <Time at={r.at} />
                      </Table.Td>
                      <Table.Td>
                        <MethodBadge method={r.method} />
                      </Table.Td>
                      <Table.Td className="mono" maw={360} style={{ wordBreak: 'break-all' }}>
                        {r.path}
                        {r.query ? `?${r.query}` : ''}
                      </Table.Td>
                      <Table.Td>
                        <StatusBadge status={r.status} />
                      </Table.Td>
                    </Table.Tr>
                  ))}
                </Table.Tbody>
              </Table>
            </div>
          )}
        </Card>
        <Stack gap="lg">
          <Card withBorder radius="md">
            <Title order={3} size="h5" mb="sm">
              Server
            </Title>
            <Table fz="sm" verticalSpacing={4}>
              <Table.Tbody>
                <Table.Tr>
                  <Table.Td c="dimmed">Version</Table.Td>
                  <Table.Td>{versionText ?? '—'}</Table.Td>
                </Table.Tr>
                {extraVersion.map(([k, v]) => (
                  <Table.Tr key={k}>
                    <Table.Td c="dimmed">{k}</Table.Td>
                    <Table.Td className="mono">{String(v)}</Table.Td>
                  </Table.Tr>
                ))}
                <Table.Tr>
                  <Table.Td c="dimmed">Storage</Table.Td>
                  <Table.Td>{i?.storage}</Table.Td>
                </Table.Tr>
                <Table.Tr>
                  <Table.Td c="dimmed">Signed in</Table.Td>
                  <Table.Td>
                    {i?.principal.name} <Badge size="sm">{i?.principal.role}</Badge>
                  </Table.Td>
                </Table.Tr>
                {i?.authDisabled && (
                  <Table.Tr>
                    <Table.Td c="dimmed">Auth</Table.Td>
                    <Table.Td>
                      <Badge color="orange">disabled</Badge>
                    </Table.Td>
                  </Table.Tr>
                )}
              </Table.Tbody>
            </Table>
          </Card>
          <Card withBorder radius="md">
            <Title order={3} size="h5" mb="sm">
              Plugins
            </Title>
            {i?.plugins.length ? (
              <Stack gap="xs">
                {i.plugins.map((p) => (
                  <div key={p.name}>
                    <Group gap="xs">
                      <Text fw={600} size="sm">
                        {p.name}
                      </Text>
                      <Badge size="xs" variant="outline">
                        {p.mount}
                      </Badge>
                      {p.admin && (
                        <Badge size="xs" variant="light">
                          admin API
                        </Badge>
                      )}
                    </Group>
                    <Text size="xs" c="dimmed">
                      {p.description}
                    </Text>
                  </div>
                ))}
              </Stack>
            ) : (
              <Text size="sm" c="dimmed">
                No provider plugins enabled.
              </Text>
            )}
          </Card>
        </Stack>
      </div>
      <MetricsCard />
    </Stack>
  );
}
