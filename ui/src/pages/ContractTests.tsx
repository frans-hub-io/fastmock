import { Accordion, ActionIcon, Alert, Box, Button, Drawer, Group, NumberInput, SegmentedControl, Select, Stack, Table, TagsInput, Text, TextInput, Title, Tooltip } from '@mantine/core';
import { IconAlertTriangle, IconBox, IconEyeOff, IconPlayerPlay, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useFeature } from '../api/features';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { ContractRun, OperationResult } from '../api/types';
import { useCan, useSpace, useSpaceChoice, useSpaceQuery } from '../auth/useAuth';
import { SpaceBadge, SpaceField } from '../components/spaces';
import { CodeEditor } from '../components/CodeEditor';
import { ResultBar, SegmentBar } from '../components/charts';
import { BodyView, DataTable, DensityToggle, DetailList, KeyValueTable, MethodBadge, PageHeader, StatusBadge, Time, ToneBadge } from '../components/common';
import { KeyValueEditor } from '../components/MatcherList';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { formatBytes } from '../lib/format';
import { isHttpUrl } from '../lib/screens';
import { formatMs } from '../lib/traffic';
import { useDensity, type Tone } from '../lib/ui';
import classes from './ContractTests.module.css';

const TONE: Record<string, Tone> = { running: 'info', passed: 'success', failed: 'danger', error: 'warning' };

function Verdict({ v }: { v: string }) {
  return <ToneBadge tone={TONE[v] ?? 'neutral'}>{v}</ToneBadge>;
}

function OperationDetail({ o }: { o: OperationResult }) {
  return (
    <Stack gap="sm">
      {o.errors.length > 0 && (
        <ul className="fm-error-list" aria-label="Problems">
          {o.errors.map((e, j) => (
            <li key={j}>{e}</li>
          ))}
        </ul>
      )}
      {o.request && (
        <Stack gap={6}>
          <span className="fm-subhead">Request</span>
          <Group gap={8} wrap="nowrap" align="flex-start">
            <MethodBadge method={o.request.method} />
            <Text size="sm" className="mono break-all">
              {o.request.url}
            </Text>
          </Group>
          <KeyValueTable data={o.request.headers} empty="no headers" />
          {o.request.body && <BodyView body={o.request.body} />}
        </Stack>
      )}
      {o.response && (
        <Stack gap={6}>
          <span className="fm-subhead">Response</span>
          <Group gap={6} wrap="wrap">
            <StatusBadge status={o.response.status} />
            {o.response.contentType && <span className="fm-chip">{o.response.contentType}</span>}
            <span className="fm-chip">{formatBytes(o.response.bodyBytes)}</span>
          </Group>
          {o.response.bodyExcerpt && <BodyView body={o.response.bodyExcerpt} contentType={o.response.contentType ?? undefined} />}
        </Stack>
      )}
      {o.topic && (
        <Text size="sm">
          Topic <span className="mono">{o.topic}</span>: {o.messages?.length ?? 0} messages
        </Text>
      )}
      {o.messages?.map((m, j) => (
        <div key={j}>
          <Text size="xs" c="dimmed">
            partition {m.partition} offset {m.offset} {m.key ? `key ${m.key}` : ''}
          </Text>
          {m.excerpt && <BodyView body={m.excerpt} />}
          {m.errors.length > 0 && (
            <ul className="fm-error-list">
              {m.errors.map((e, k) => (
                <li key={k}>{e}</li>
              ))}
            </ul>
          )}
        </div>
      ))}
    </Stack>
  );
}

function RunDetail({ id }: { id: string }) {
  const [show, setShow] = useState<'all' | 'problems'>('all');
  const q = useQuery({
    queryKey: ['contract-tests', id],
    queryFn: () => unwrap(client.GET('/__admin/contract-tests/{id}', { params: { path: { id } } })),
    refetchInterval: (query) => (query.state.data?.status === 'running' ? 1500 : false),
  });
  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} />;
  const r = q.data;
  if (!r) return null;
  const results = r.results ?? [];
  const problems = results.filter((o) => o.verdict !== 'passed');
  const shown = show === 'all' ? results : problems;
  return (
    <Stack gap="md" data-testid="contract-run">
      <div className={classes.runHead} data-status={r.status}>
        <Group gap="xs" wrap="wrap">
          <Verdict v={r.status} />
          <span className="fm-chip">{r.kind}</span>
          {r.durationMs != null && <span className="fm-chip">{formatMs(r.durationMs)}</span>}
        </Group>
        <Title order={3} size="h4" mt={6} className="break-all">
          {r.service ?? r.specRef ?? 'Pasted specification'}
        </Title>
        {r.targetUrl && (
          <Text size="sm" className="mono break-all" c="dimmed">
            → {r.targetUrl}
          </Text>
        )}
        <div className={classes.big}>
          <span className={classes.bigValue}>
            {r.summary.passed}
            <span className={classes.bigOf}>/{r.summary.total}</span>
          </span>
          <span className={classes.bigLabel}>checks passed</span>
        </div>
        <SegmentBar
          label="Results"
          testId="contract-summary"
          parts={[
            { key: 'passed', label: 'Passed', value: r.summary.passed, tone: 'success' },
            { key: 'failed', label: 'Failed', value: r.summary.failed, tone: 'danger' },
            { key: 'errors', label: 'Errors', value: r.summary.errors, tone: 'warning' },
          ]}
        />
      </div>
      <DetailList
        items={[
          { label: 'Started', value: <Time at={r.startedAt} /> },
          { label: 'Started by', value: r.startedBy },
          { label: 'Finished', value: <Time at={r.finishedAt} />, hidden: !r.finishedAt },
          { label: 'Specification', value: <span className="mono">{r.specRef}</span>, hidden: !r.specRef },
        ]}
      />
      {r.error && (
        <Alert color="red" icon={<IconAlertTriangle size={18} />} title="The run stopped">
          {r.error}
        </Alert>
      )}
      {r.detailsHidden && (
        <Alert color="gray" icon={<IconEyeOff size={18} />} data-testid="contract-details-hidden">
          Request and response excerpts, message contents and error texts are visible to editors only.
        </Alert>
      )}
      {results.length > 0 && (
        <Group justify="space-between" wrap="wrap" gap="xs">
          <Title order={4} size="h6" className="fm-subhead">
            Operations
          </Title>
          <SegmentedControl
            size="xs"
            value={show}
            onChange={(v) => setShow(v)}
            data={[
              { label: `All ${String(results.length)}`, value: 'all' },
              { label: `Problems ${String(problems.length)}`, value: 'problems' },
            ]}
            aria-label="Which operations"
          />
        </Group>
      )}
      {show === 'problems' && !problems.length ? (
        <EmptyState compact title="Every check passed" icon={<IconBox size={20} />} />
      ) : (
        <Accordion multiple variant="separated" radius="md" classNames={{ item: classes.item, control: classes.control }}>
          {shown.map((o) => {
            const i = results.indexOf(o);
            return (
              <Accordion.Item key={i} value={String(i)} data-verdict={o.verdict}>
                <Accordion.Control>
                  <Group gap="xs" wrap="nowrap">
                    <Verdict v={o.verdict} />
                    <Text size="sm" fw={600} className="mono truncate">
                      {o.operation}
                    </Text>
                    {o.example && <span className="fm-chip fm-chip-soft">{o.example}</span>}
                    {o.errors.length > 0 && (
                      <Text size="xs" c="dimmed" className="fm-nowrap" visibleFrom="sm">
                        {o.errors.length} problem{o.errors.length === 1 ? '' : 's'}
                      </Text>
                    )}
                    <Text size="xs" c="dimmed" ml="auto" className="tabular fm-nowrap">
                      {formatMs(o.durationMs)}
                    </Text>
                  </Group>
                </Accordion.Control>
                <Accordion.Panel>
                  <OperationDetail o={o} />
                </Accordion.Panel>
              </Accordion.Item>
            );
          })}
        </Accordion>
      )}
    </Stack>
  );
}

function parseSpec(text: string): unknown {
  try {
    return JSON.parse(text) as unknown;
  } catch {
    return text;
  }
}

function StartForm({ onStarted }: { onStarted: (id: string) => void }) {
  const services = useQuery({
    queryKey: ['services', 'list', {}],
    queryFn: () => unwrap(client.GET('/__admin/services')),
    enabled: useFeature('/__admin/services', 'get') === 'available',
  });
  const [source, setSource] = useState<'ref' | 'spec'>('ref');
  const [specRef, setSpecRef] = useState<string | null>(null);
  const [spec, setSpec] = useState('');
  const [targetUrl, setTargetUrl] = useState('');
  const [operations, setOperations] = useState<string[]>([]);
  const [headers, setHeaders] = useState<Record<string, string> | undefined>();
  const [secretHeaders, setSecretHeaders] = useState<Record<string, string> | undefined>();
  const [timeoutMs, setTimeoutMs] = useState<number | ''>('');
  const [space, setSpace] = useSpaceChoice('contracts:write');
  const canSecrets = useCan()('contracts:secrets', space);
  const start = useApiMutation<void, { id: string }>({
    fn: () =>
      unwrap(
        client.POST('/__admin/contract-tests', {
          params: { query: space ? { space } : {} },
          body: {
            ...(source === 'ref' ? { specRef } : { spec: parseSpec(spec) as never }),
            ...(targetUrl ? { targetUrl } : {}),
            ...(operations.length ? { operations } : {}),
            ...(headers ? { headers } : {}),
            ...(secretHeaders && canSecrets ? { secretHeaders } : {}),
            ...(timeoutMs ? { timeoutMs } : {}),
          },
        }),
      ),
    invalidate: [['contract-tests']],
    success: 'Contract test started',
    onSuccess: (d) => onStarted(d.id),
  });
  const picked = services.data?.services.find((s) => s.id === specRef);
  const urlError = targetUrl && !isHttpUrl(targetUrl) ? 'An absolute http:// or https:// URL' : undefined;
  const ready = source === 'ref' ? !!specRef : !!spec.trim();
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (ready && !urlError) start.mutate();
      }}
    >
      <Stack gap="sm">
        <SpaceField perm="contracts:write" value={space} onChange={setSpace} />
        <SegmentedControl
          value={source}
          onChange={(v) => setSource(v)}
          data={[
            { label: 'Imported service', value: 'ref' },
            { label: 'Paste a specification', value: 'spec' },
          ]}
          w="fit-content"
          maw="100%"
          aria-label="Specification source"
        />
        {source === 'ref' ? (
          <Select
            label="Service"
            description="Services whose document was stored when they were imported"
            data={(services.data?.services ?? []).filter((s) => s.hasSpec !== false).map((s) => ({ value: s.id, label: `${s.name} ${s.version}` }))}
            value={specRef}
            onChange={setSpecRef}
            searchable
            placeholder={services.isLoading ? 'Loading…' : 'Pick a service'}
            nothingFoundMessage="No services"
          />
        ) : (
          <CodeEditor value={spec} onChange={setSpec} language={spec.trimStart().startsWith('{') ? 'json' : 'yaml'} minHeight={160} label="OpenAPI or AsyncAPI document" placeholder="openapi: 3.0.3 …" />
        )}
        <TextInput
          label="Target URL"
          description="The implementation under test (HTTP APIs). Outbound calls obey the egress policy."
          placeholder="https://service.example.com"
          value={targetUrl}
          onChange={(e) => setTargetUrl(e.currentTarget.value)}
          error={urlError}
          classNames={{ input: 'mono' }}
        />
        <TagsInput
          label="Operations"
          description="Empty = all operations"
          value={operations}
          onChange={setOperations}
          data={picked ? picked.operations.map((o) => o.name) : []}
          placeholder={picked ? 'Pick or type operation names' : undefined}
        />
        <KeyValueEditor label="Headers" value={headers} onChange={setHeaders} />
        {canSecrets && (
          <KeyValueEditor label="Secret headers: header name → secret name (contracts:secrets permission; FASTMOCK_CONTRACT_SECRETS binds each secret to its hosts)" value={secretHeaders} onChange={setSecretHeaders} />
        )}
        <NumberInput label="Timeout per call" suffix=" ms" value={timeoutMs} onChange={(v) => setTimeoutMs(typeof v === 'number' ? v : '')} min={100} max={120000} w={220} />
        <Group justify="flex-end">
          <Button type="submit" leftSection={<IconPlayerPlay size={16} />} loading={start.isPending} disabled={!ready || !!urlError} data-testid="start-contract-test">
            Start
          </Button>
        </Group>
      </Stack>
    </form>
  );
}

function runTitle(r: ContractRun): string {
  return r.service ?? r.specRef ?? 'Pasted specification';
}

function ContractTestsInner() {
  const can = useCan();
  const canEdit = can('contracts:write');
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [selected, setSelected] = useState<string | null>(null);
  const [startOpen, setStartOpen] = useState(false);
  const [density, setDensity] = useDensity();
  const [status, setStatus] = useState('all');
  const q = useQuery({
    queryKey: ['contract-tests', spaceQ],
    queryFn: () => unwrap(client.GET('/__admin/contract-tests', { params: { query: { limit: 100, ...spaceQ } } })),
    refetchInterval: (query) => (query.state.data?.runs.some((r) => r.status === 'running') ? 2000 : 10000),
  });
  const del = useApiMutation<string>({ fn: (id) => unwrap(client.DELETE('/__admin/contract-tests/{id}', { params: { path: { id } } })), invalidate: [['contract-tests']], success: 'Run deleted' });
  const all = q.data?.runs ?? [];
  const runs = status === 'all' ? all : all.filter((r) => (status === 'failed' ? r.status === 'failed' || r.status === 'error' : r.status === status));
  const counts = { passed: all.filter((r) => r.status === 'passed').length, failed: all.filter((r) => r.status === 'failed' || r.status === 'error').length, running: all.filter((r) => r.status === 'running').length };
  return (
    <Stack gap="md">
      <PageHeader
        title="Contract tests"
        description="Call a real implementation with the examples of an imported specification and check its answers against the schemas (Microcks-style conformance)."
        meta={
          all.length > 0 && (
            <>
              <span className="fm-status" data-tone="success">
                {counts.passed} passed
              </span>
              <span className="fm-status" data-tone={counts.failed ? 'danger' : 'neutral'}>
                {counts.failed} failed
              </span>
              {counts.running > 0 && (
                <span className="fm-status" data-tone="info">
                  {counts.running} running
                </span>
              )}
            </>
          )
        }
        actions={
          canEdit && (
            <Button leftSection={<IconPlayerPlay size={16} />} onClick={() => setStartOpen(true)} data-testid="new-contract-run">
              New run
            </Button>
          )
        }
      />
      {all.length > 0 && (
        <div className="fm-toolbar">
          <SegmentedControl
            value={status}
            onChange={setStatus}
            data={[
              { label: 'All', value: 'all' },
              { label: 'Passed', value: 'passed' },
              { label: 'Failed', value: 'failed' },
              { label: 'Running', value: 'running' },
            ]}
            aria-label="Status filter"
          />
          <Box className="fm-grow" visibleFrom="sm" />
          <DensityToggle value={density} onChange={setDensity} />
        </div>
      )}
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : all.length > 0 && !runs.length ? (
        <EmptyState
          title="No runs with this status"
          icon={<IconBox size={28} />}
          action={
            <Button variant="default" onClick={() => setStatus('all')}>
              Show all runs
            </Button>
          }
        />
      ) : !runs.length ? (
        <EmptyState
          title="No runs yet"
          icon={<IconBox size={30} />}
          docs={{ label: 'Contract testing guide', href: docsUrl('guide/contract-testing') }}
          action={
            canEdit ? (
              <Button leftSection={<IconPlayerPlay size={16} />} onClick={() => setStartOpen(true)}>
                Start a contract test
              </Button>
            ) : undefined
          }
        >
          A run sends the examples of an OpenAPI or AsyncAPI document to the service under test and checks every answer against the schemas.
        </EmptyState>
      ) : (
        <DataTable testId="contract-runs" miw={900} density={density}>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Started</Table.Th>
              <Table.Th>Status</Table.Th>
              <Table.Th>Service and target</Table.Th>
              <Table.Th w={180}>Result</Table.Th>
              <Table.Th>By</Table.Th>
              {spaces.enabled && <Table.Th>Space</Table.Th>}
              <Table.Th w={56}>
                <span className="sr-only">Actions</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {runs.map((r) => (
              <Table.Tr
                key={r.id}
                className={`clickable-row ${selected === r.id ? 'fm-row-selected' : ''}`}
                tabIndex={0}
                aria-label={`${runTitle(r)} ${r.status}`}
                onClick={() => setSelected(r.id)}
                onKeyDown={(e) => {
                  if (e.target !== e.currentTarget) return;
                  if (e.key === 'Enter' || e.key === ' ') {
                    e.preventDefault();
                    setSelected(r.id);
                  }
                }}
                data-testid="contract-run-row"
              >
                <Table.Td data-label="Started">
                  <Time at={r.startedAt} />
                </Table.Td>
                <Table.Td data-label="Status">
                  <Verdict v={r.status} />
                </Table.Td>
                <Table.Td>
                  <Stack gap={2} miw={0}>
                    <Group gap={6} wrap="nowrap">
                      <Text size="sm" fw={600} className="truncate">
                        {runTitle(r)}
                      </Text>
                      <span className="fm-chip">{r.kind}</span>
                    </Group>
                    {r.targetUrl && (
                      <Text size="xs" c="dimmed" className="mono break-all">
                        → {r.targetUrl}
                      </Text>
                    )}
                  </Stack>
                </Table.Td>
                <Table.Td data-label="Result">
                  <ResultBar passed={r.summary.passed} failed={r.summary.failed} errors={r.summary.errors} total={r.summary.total} />
                </Table.Td>
                <Table.Td data-label="By">{r.startedBy}</Table.Td>
                {spaces.enabled && (
                  <Table.Td data-label="Space">
                    <SpaceBadge space={r.space} />
                  </Table.Td>
                )}
                <Table.Td onClick={(e) => e.stopPropagation()} data-empty={can('contracts:write', r.space ?? null) ? undefined : true}>
                  {can('contracts:write', r.space ?? null) && (
                    <Group justify="flex-end">
                      <Tooltip label="Delete run">
                        <ActionIcon variant="subtle" color="red" aria-label="Delete run" onClick={() => confirmAction({ title: 'Delete run?', message: 'The run and its results are removed.', confirm: 'Delete', onConfirm: () => del.mutate(r.id) })}>
                          <IconTrash size={16} />
                        </ActionIcon>
                      </Tooltip>
                    </Group>
                  )}
                </Table.Td>
              </Table.Tr>
            ))}
          </Table.Tbody>
        </DataTable>
      )}
      <Drawer opened={!!selected} onClose={() => setSelected(null)} position="right" size="xl" title="Contract test run">
        {selected && <RunDetail id={selected} />}
      </Drawer>
      {canEdit && (
        <Drawer opened={startOpen} onClose={() => setStartOpen(false)} position="right" size="lg" title="New contract test run">
          <StartForm
            onStarted={(id) => {
              setStartOpen(false);
              setSelected(id);
            }}
          />
        </Drawer>
      )}
    </Stack>
  );
}

export default function ContractTests() {
  return (
    <FeatureGate path="/__admin/contract-tests" method="get" what="Contract testing">
      <ContractTestsInner />
    </FeatureGate>
  );
}
