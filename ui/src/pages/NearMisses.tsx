import { Anchor, Button, Group, Paper, Progress, Select, Stack, Text, Textarea, TextInput, UnstyledButton } from '@mantine/core';
import { IconAlertTriangle, IconCircleCheck, IconListSearch, IconSearch, IconSend, IconX } from '@tabler/icons-react';
import { useMutation, useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { Link, useSearchParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { useSpaceQuery } from '../auth/useAuth';
import type { NearMiss, RequestPattern } from '../api/types';
import { DiffView } from '../components/DiffView';
import { MethodBadge, PageHeader, PathText, SectionCard, StatusBadge, Time, ToneBadge } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { headerLines, parseHeaderLines } from '../lib/curl';
import { docsUrl } from '../lib/docs';
import { stubFromJournal } from '../lib/stubs';
import classes from './NearMisses.module.css';

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
  const tone = pct >= 75 ? 'warning' : 'neutral';
  return (
    <Paper withBorder radius="md" p="md" data-testid="near-miss" className={classes.miss}>
      <Group justify="space-between" mb="sm" wrap="wrap" gap="sm">
        <Group gap="xs" miw={0}>
          <Anchor component={Link} to={`/stubs/${miss.stubId}`} fw={700}>
            {miss.name ?? miss.stubId}
          </Anchor>
          <ToneBadge tone={tone}>{pct}% match</ToneBadge>
        </Group>
        <Progress value={pct} w={160} size="sm" radius="xl" aria-label={`${String(pct)}% of matchers matched`} />
      </Group>
      {miss.mismatches.length > 0 && (
        <ul className={classes.mismatches}>
          {miss.mismatches.map((m, i) => (
            <li key={i}>
              <IconX size={14} aria-hidden className={classes.x} />
              <span>{m}</span>
            </li>
          ))}
        </ul>
      )}
      {stub.isLoading ? (
        <LoadingState />
      ) : stub.data ? (
        <DiffView left={stub.data.request} right={actual} leftLabel="stub expects" rightLabel="request had" />
      ) : null}
    </Paper>
  );
}

export default function NearMisses() {
  const [params, setParams] = useSearchParams();
  const selectedId = params.get('id');
  const spaceQ = useSpaceQuery();
  const recent = useQuery({
    queryKey: ['requests', 'unmatched'],
    queryFn: () => unwrap(client.GET('/__admin/requests/unmatched', { params: { query: { limit: 50 } } })),
    refetchInterval: 5000,
  });
  const [probe, setProbe] = useState<Probe>({ method: 'GET', path: '/', headers: {} });
  const [headersText, setHeadersText] = useState('');
  const run = useMutation({
    mutationFn: (p: Probe) => unwrap(client.POST('/__admin/requests/near-misses', { body: p, params: { query: spaceQ } })),
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
      <div className={classes.layout}>
        <Stack gap="md">
          <SectionCard title="Recent unmatched requests" description="Pick one to explain it" icon={<IconAlertTriangle size={16} />} padding="sm">
            {recent.isLoading ? (
              <LoadingState />
            ) : recent.isError ? (
              <ErrorState error={recent.error} />
            ) : !recent.data?.requests.length ? (
              <EmptyState compact title="No unmatched requests" icon={<IconCircleCheck size={22} />}>
                Every request matched a stub.
              </EmptyState>
            ) : (
              <ul className={classes.recent}>
                {recent.data.requests.map((r) => {
                  const sel = String(r.id) === selectedId;
                  return (
                    <li key={r.id}>
                      <UnstyledButton
                        className={`${classes.recentRow} clickable-row ${sel ? 'fm-row-selected' : ''}`}
                        aria-pressed={sel}
                        onClick={() => setParams({ id: String(r.id) }, { replace: true })}
                      >
                        <MethodBadge method={r.method} />
                        <span className={classes.recentPath}>
                          <PathText path={r.path} query={r.query} />
                        </span>
                        <StatusBadge status={r.status} />
                        <span className={classes.recentTime}>
                          <Time at={r.at} />
                        </span>
                      </UnstyledButton>
                    </li>
                  );
                })}
              </ul>
            )}
          </SectionCard>
          <SectionCard title="Try a request" description="Describe a request to see how close the stubs come" icon={<IconSend size={16} />}>
            <Stack gap="xs">
              <Group gap="xs" wrap="nowrap">
                <Select
                  data={['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS']}
                  value={probe.method}
                  onChange={(v) => v && setProbe({ ...probe, method: v })}
                  w={116}
                  aria-label="Method"
                  allowDeselect={false}
                />
                <TextInput value={probe.path} onChange={(e) => setProbe({ ...probe, path: e.currentTarget.value })} aria-label="Path" className="fm-grow" classNames={{ input: 'mono' }} />
              </Group>
              <TextInput value={probe.query ?? ''} onChange={(e) => setProbe({ ...probe, query: e.currentTarget.value })} placeholder="query, e.g. a=1&b=2" aria-label="Query" classNames={{ input: 'mono' }} />
              <Textarea value={headersText} onChange={(e) => setHeadersText(e.currentTarget.value)} placeholder="Header: value" autosize minRows={2} maxRows={6} aria-label="Headers" classNames={{ input: 'mono' }} />
              <Textarea value={probe.body ?? ''} onChange={(e) => setProbe({ ...probe, body: e.currentTarget.value })} placeholder="body" autosize minRows={2} maxRows={10} aria-label="Body" classNames={{ input: 'mono' }} />
              <Group>
                <Button leftSection={<IconSearch size={16} />} onClick={submit} loading={run.isPending}>
                  Explain
                </Button>
              </Group>
            </Stack>
          </SectionCard>
        </Stack>
        <Stack gap="md">
          {run.isPending ? (
            <LoadingState />
          ) : run.isError ? (
            <ErrorState error={run.error} />
          ) : run.data ? (
            run.data.nearMisses.length ? (
              <>
                <Text size="sm" c="dimmed">
                  {run.data.nearMisses.length} closest stub{run.data.nearMisses.length === 1 ? '' : 's'} for{' '}
                  <Text span fw={600} c="var(--mantine-color-text)">
                    {probe.method} <PathText path={probe.path} />
                  </Text>
                </Text>
                {run.data.nearMisses.map((m) => (
                  <NearMissCard key={m.stubId} miss={m} probe={probe} />
                ))}
              </>
            ) : (
              <Paper withBorder radius="md">
                <EmptyState title="No stubs to compare" icon={<IconListSearch size={30} />} docs={{ label: 'How request matching works', href: docsUrl('guide/request-matching') }}>
                  There are no stubs, or none is close to this request.
                </EmptyState>
              </Paper>
            )
          ) : (
            <Paper withBorder radius="md">
              <EmptyState title="Pick a request" icon={<IconListSearch size={30} />} docs={{ label: 'How request matching works', href: docsUrl('guide/request-matching') }}>
                Select an unmatched request or describe one to see the closest stubs and what they expect.
              </EmptyState>
            </Paper>
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
