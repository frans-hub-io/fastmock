import { Anchor, Badge, Button, Card, Group, List, Paper, Progress, Select, Stack, Table, Text, Textarea, TextInput, Title } from '@mantine/core';
import { IconSearch } from '@tabler/icons-react';
import { useMutation, useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { Link, useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import type { NearMiss, RequestPattern } from '../api/types';
import { DiffView } from '../components/DiffView';
import { MethodBadge, PageHeader, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { headerLines, parseHeaderLines } from '../lib/curl';
import { stubFromJournal } from '../lib/stubs';

interface Probe {
  method: string;
  path: string;
  query?: string;
  headers: Record<string, string>;
  body?: string;
}

function NearMissCard({ miss, probe }: { miss: NearMiss; probe: Probe }) {
  const stub = useQuery({ queryKey: ['stub', miss.stubId], queryFn: () => unwrap(client.GET('/__admin/stubs/{id}', { params: { path: { id: miss.stubId } } })), retry: false });
  const actual: RequestPattern = stubFromJournal({
    id: 0,
    at: '',
    source: '',
    method: probe.method,
    path: probe.path,
    query: probe.query ?? '',
    headers: probe.headers,
    body: probe.body ?? '',
    status: 0,
    responseBody: '',
    matched: false,
    durationMs: 0,
  }).request;
  const pct = Math.round(miss.score * 100);
  return (
    <Card withBorder radius="md" data-testid="near-miss">
      <Group justify="space-between" mb="xs" wrap="wrap">
        <Group gap="xs">
          <Anchor component={Link} to={`/stubs/${miss.stubId}`} fw={600}>
            {miss.name ?? miss.stubId}
          </Anchor>
          <Badge variant="light" color={pct >= 75 ? 'yellow' : 'gray'}>
            {pct}% match
          </Badge>
        </Group>
        <Progress value={pct} w={160} aria-label={`${pct}% of matchers matched`} />
      </Group>
      {miss.mismatches.length > 0 && (
        <List size="sm" spacing={2} mb="sm" c="red">
          {miss.mismatches.map((m, i) => (
            <List.Item key={i}>{m}</List.Item>
          ))}
        </List>
      )}
      {stub.isLoading ? (
        <LoadingState />
      ) : stub.data ? (
        <DiffView left={stub.data.request} right={actual} leftLabel="stub expects" rightLabel="request had" />
      ) : null}
    </Card>
  );
}

export default function NearMisses() {
  const [params, setParams] = useSearchParams();
  const selectedId = params.get('id');
  const recent = useQuery({
    queryKey: ['requests', 'unmatched'],
    queryFn: () => unwrap(client.GET('/__admin/requests/unmatched', { params: { query: { limit: 50 } } })),
    refetchInterval: 5000,
  });
  const [probe, setProbe] = useState<Probe>({ method: 'GET', path: '/', headers: {} });
  const [headersText, setHeadersText] = useState('');
  const run = useMutation({
    mutationFn: (p: Probe) => unwrap(client.POST('/__admin/requests/near-misses', { body: p })),
  });

  useEffect(() => {
    if (!selectedId || !recent.data) return;
    const e = recent.data.requests.find((r) => String(r.id) === selectedId);
    if (!e) return;
    const p: Probe = { method: e.method, path: e.path, query: e.query, headers: e.headers, body: e.body };
    setProbe(p);
    setHeadersText(headerLines(e.headers));
    run.mutate(p);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [selectedId, recent.data === undefined]);

  const submit = () => {
    const p = { ...probe, headers: parseHeaderLines(headersText) };
    setProbe(p);
    run.mutate(p);
  };

  return (
    <Stack gap="md">
      <PageHeader title="Near misses" description="Why a request did not match: the closest stubs, which of their conditions failed, and a diff of what they expect." />
      <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fit, minmax(360px, 1fr))', gap: 'var(--mantine-spacing-lg)', alignItems: 'start' }}>
        <Stack gap="md">
          <Paper withBorder p="md" radius="md">
            <Title order={3} size="h5" mb="sm">
              Recent unmatched requests
            </Title>
            {recent.isLoading ? (
              <LoadingState />
            ) : recent.isError ? (
              <ErrorState error={recent.error} />
            ) : !recent.data?.requests.length ? (
              <EmptyState title="No unmatched requests">Every request matched a stub.</EmptyState>
            ) : (
              <div className="table-scroll" style={{ maxHeight: 360, overflowY: 'auto' }}>
                <Table fz="sm" highlightOnHover>
                  <Table.Tbody>
                    {recent.data.requests.map((r) => (
                      <Table.Tr
                        key={r.id}
                        className="clickable-row"
                        tabIndex={0}
                        bg={String(r.id) === selectedId ? 'var(--mantine-primary-color-light)' : undefined}
                        onClick={() => setParams({ id: String(r.id) }, { replace: true })}
                        onKeyDown={(e) => {
                          if (e.key === 'Enter') setParams({ id: String(r.id) }, { replace: true });
                        }}
                      >
                        <Table.Td>
                          <Time at={r.at} />
                        </Table.Td>
                        <Table.Td>
                          <MethodBadge method={r.method} />
                        </Table.Td>
                        <Table.Td className="mono" style={{ wordBreak: 'break-all' }}>
                          {r.path}
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
          </Paper>
          <Paper withBorder p="md" radius="md">
            <Title order={3} size="h5" mb="sm">
              Try a request
            </Title>
            <Stack gap="xs">
              <Group gap="xs" wrap="nowrap">
                <Select
                  data={['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS']}
                  value={probe.method}
                  onChange={(v) => v && setProbe({ ...probe, method: v })}
                  w={110}
                  aria-label="Method"
                  allowDeselect={false}
                />
                <TextInput value={probe.path} onChange={(e) => setProbe({ ...probe, path: e.currentTarget.value })} aria-label="Path" style={{ flex: 1 }} className="mono" />
              </Group>
              <TextInput value={probe.query ?? ''} onChange={(e) => setProbe({ ...probe, query: e.currentTarget.value })} placeholder="query, e.g. a=1&b=2" aria-label="Query" />
              <Textarea value={headersText} onChange={(e) => setHeadersText(e.currentTarget.value)} placeholder="Header: value" autosize minRows={2} maxRows={6} aria-label="Headers" classNames={{ input: 'mono' }} />
              <Textarea value={probe.body ?? ''} onChange={(e) => setProbe({ ...probe, body: e.currentTarget.value })} placeholder="body" autosize minRows={2} maxRows={10} aria-label="Body" classNames={{ input: 'mono' }} />
              <Group>
                <Button leftSection={<IconSearch size={16} />} onClick={submit} loading={run.isPending}>
                  Explain
                </Button>
              </Group>
            </Stack>
          </Paper>
        </Stack>
        <Stack gap="md">
          {run.isPending ? (
            <LoadingState />
          ) : run.isError ? (
            <ErrorState error={run.error} />
          ) : run.data ? (
            run.data.nearMisses.length ? (
              run.data.nearMisses.map((m) => <NearMissCard key={m.stubId} miss={m} probe={probe} />)
            ) : (
              <EmptyState title="No stubs to compare">There are no stubs, or none is close to this request.</EmptyState>
            )
          ) : (
            <EmptyState title="Pick a request">Select an unmatched request or describe one to see the closest stubs.</EmptyState>
          )}
          {run.data && (
            <Text size="xs" c="dimmed">
              Score = share of a stub&apos;s conditions that matched this request.
            </Text>
          )}
        </Stack>
      </div>
    </Stack>
  );
}
