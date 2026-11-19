import { Accordion, ActionIcon, Alert, Badge, Button, Drawer, Group, NumberInput, Paper, SegmentedControl, Select, Stack, Table, TagsInput, Text, TextInput, Title } from '@mantine/core';
import { IconPlayerPlay, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { useFeature } from '../api/features';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { BodyView, KeyValueTable, PageHeader, Time } from '../components/common';
import { KeyValueEditor } from '../components/MatcherList';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';

const COLOR: Record<string, string> = { running: 'blue', passed: 'green', failed: 'red', error: 'orange' };

function RunDetail({ id }: { id: string }) {
  const q = useQuery({
    queryKey: ['contract-tests', id],
    queryFn: () => unwrap(client.GET('/__admin/contract-tests/{id}', { params: { path: { id } } })),
    refetchInterval: (query) => (query.state.data?.status === 'running' ? 1500 : false),
  });
  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} />;
  const r = q.data;
  if (!r) return null;
  return (
    <Stack gap="sm">
      <Group gap="xs">
        <Badge color={COLOR[r.status]}>{r.status}</Badge>
        <Badge variant="outline">{r.kind}</Badge>
        <Text size="sm">
          {r.summary.passed}/{r.summary.total} passed · {r.summary.failed} failed · {r.summary.errors} errors
        </Text>
      </Group>
      <Text size="sm" className="mono">
        {r.targetUrl ?? r.service ?? r.specRef}
      </Text>
      {r.error && (
        <Text c="red" size="sm">
          {r.error}
        </Text>
      )}
      {r.detailsHidden && (
        <Alert color="gray" data-testid="contract-details-hidden">
          Request and response excerpts, message contents and error texts are visible to editors only.
        </Alert>
      )}
      <Accordion multiple variant="separated">
        {(r.results ?? []).map((o, i) => (
          <Accordion.Item key={i} value={String(i)}>
            <Accordion.Control>
              <Group gap="xs" wrap="nowrap">
                <Badge color={COLOR[o.verdict]} size="sm">
                  {o.verdict}
                </Badge>
                <Text size="sm" fw={500}>
                  {o.operation}
                </Text>
                {o.example && (
                  <Text size="xs" c="dimmed">
                    {o.example}
                  </Text>
                )}
                <Text size="xs" c="dimmed" ml="auto">
                  {o.durationMs} ms
                </Text>
              </Group>
            </Accordion.Control>
            <Accordion.Panel>
              <Stack gap="xs">
                {o.errors.map((e, j) => (
                  <Text key={j} size="sm" c="red">
                    {e}
                  </Text>
                ))}
                {o.request && (
                  <>
                    <Text size="sm" fw={600}>
                      Request: {o.request.method} <span className="mono">{o.request.url}</span>
                    </Text>
                    <KeyValueTable data={o.request.headers} />
                    {o.request.body && <BodyView body={o.request.body} />}
                  </>
                )}
                {o.response && (
                  <>
                    <Text size="sm" fw={600}>
                      Response: {o.response.status} {o.response.contentType ?? ''} ({o.response.bodyBytes} bytes)
                    </Text>
                    {o.response.bodyExcerpt && <BodyView body={o.response.bodyExcerpt} contentType={o.response.contentType ?? undefined} />}
                  </>
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
                    {m.errors.map((e, k) => (
                      <Text key={k} size="xs" c="red">
                        {e}
                      </Text>
                    ))}
                  </div>
                ))}
              </Stack>
            </Accordion.Panel>
          </Accordion.Item>
        ))}
      </Accordion>
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

function StartForm() {
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
  const start = useApiMutation<void, { id: string }>({
    fn: () =>
      unwrap(client.POST('/__admin/contract-tests', { body: {
        ...(source === 'ref' ? { specRef } : { spec: parseSpec(spec) as never }),
        ...(targetUrl ? { targetUrl } : {}),
        ...(operations.length ? { operations } : {}),
        ...(headers ? { headers } : {}),
        ...(secretHeaders ? { secretHeaders } : {}),
        ...(timeoutMs ? { timeoutMs } : {}),
      } })),
    invalidate: [['contract-tests']],
    success: 'Contract test started',
  });
  return (
    <Paper withBorder p="md" radius="md">
      <Title order={3} size="h5" mb="sm">
        New run
      </Title>
      <Stack gap="sm">
        <SegmentedControl
          value={source}
          onChange={(v) => setSource(v)}
          data={[
            { label: 'Imported service', value: 'ref' },
            { label: 'Paste a specification', value: 'spec' },
          ]}
          w="fit-content"
        />
        {source === 'ref' ? (
          <Select
            label="Service"
            data={(services.data?.services ?? []).map((s) => ({ value: s.id, label: `${s.name} ${s.version}` }))}
            value={specRef}
            onChange={setSpecRef}
            searchable
            placeholder={services.isLoading ? 'Loading…' : 'Pick a service'}
            nothingFoundMessage="No services"
          />
        ) : (
          <CodeEditor value={spec} onChange={setSpec} language={spec.trimStart().startsWith('{') ? 'json' : 'yaml'} minHeight={160} label="OpenAPI or AsyncAPI document" placeholder="openapi: 3.0.3 …" />
        )}
        <TextInput label="Target URL" description="The implementation under test (HTTP APIs). Outbound calls obey the egress policy." placeholder="https://service.example.com" value={targetUrl} onChange={(e) => setTargetUrl(e.currentTarget.value)} />
        <TagsInput label="Operations" description="Empty = all operations" value={operations} onChange={setOperations} />
        <KeyValueEditor label="Headers" value={headers} onChange={setHeaders} />
        <KeyValueEditor label="Secret headers: header name → secret name (admin role; FASTMOCK_CONTRACT_SECRETS binds each secret to its hosts)" value={secretHeaders} onChange={setSecretHeaders} />
        <NumberInput label="Timeout per call (ms)" value={timeoutMs} onChange={(v) => setTimeoutMs(typeof v === 'number' ? v : '')} min={100} max={120000} w={220} />
        <Group>
          <Button leftSection={<IconPlayerPlay size={16} />} loading={start.isPending} disabled={source === 'ref' ? !specRef : !spec.trim()} onClick={() => start.mutate()}>
            Start
          </Button>
        </Group>
      </Stack>
    </Paper>
  );
}

function ContractTestsInner() {
  const canEdit = useCan('editor');
  const [selected, setSelected] = useState<string | null>(null);
  const q = useQuery({
    queryKey: ['contract-tests'],
    queryFn: () => unwrap(client.GET('/__admin/contract-tests', { params: { query: { limit: 100 } } })),
    refetchInterval: (query) => (query.state.data?.runs.some((r) => r.status === 'running') ? 2000 : 10000),
  });
  const del = useApiMutation<string>({ fn: (id) => unwrap(client.DELETE('/__admin/contract-tests/{id}', { params: { path: { id } } })), invalidate: [['contract-tests']], success: 'Run deleted' });
  return (
    <Stack gap="md">
      <PageHeader title="Contract tests" description="Call a real implementation with the examples of an imported specification and check its answers against the schemas (Microcks-style conformance)." />
      {canEdit && <StartForm />}
      <Title order={3} size="h5">
        History
      </Title>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} />
      ) : !q.data?.runs.length ? (
        <EmptyState title="No runs yet" />
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm" highlightOnHover miw={820}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Started</Table.Th>
                  <Table.Th>Status</Table.Th>
                  <Table.Th>Kind</Table.Th>
                  <Table.Th>Service / target</Table.Th>
                  <Table.Th>Result</Table.Th>
                  <Table.Th>By</Table.Th>
                  <Table.Th />
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {q.data.runs.map((r) => (
                  <Table.Tr key={r.id} className="clickable-row" tabIndex={0} onClick={() => setSelected(r.id)} onKeyDown={(e) => e.key === 'Enter' && setSelected(r.id)}>
                    <Table.Td>
                      <Time at={r.startedAt} />
                    </Table.Td>
                    <Table.Td>
                      <Badge color={COLOR[r.status]}>{r.status}</Badge>
                    </Table.Td>
                    <Table.Td>{r.kind}</Table.Td>
                    <Table.Td className="mono" maw={320} style={{ wordBreak: 'break-all' }}>
                      {r.service ?? r.specRef ?? '—'} {r.targetUrl ? `→ ${r.targetUrl}` : ''}
                    </Table.Td>
                    <Table.Td>
                      {r.summary.passed}/{r.summary.total}
                    </Table.Td>
                    <Table.Td>{r.startedBy}</Table.Td>
                    <Table.Td onClick={(e) => e.stopPropagation()}>
                      {canEdit && (
                        <ActionIcon variant="subtle" color="red" aria-label="Delete run" onClick={() => confirmAction({ title: 'Delete run?', message: 'The run and its results are removed.', confirm: 'Delete', onConfirm: () => del.mutate(r.id) })}>
                          <IconTrash size={16} />
                        </ActionIcon>
                      )}
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
      <Drawer opened={!!selected} onClose={() => setSelected(null)} position="right" size="xl" title="Contract test run">
        {selected && <RunDetail id={selected} />}
      </Drawer>
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
