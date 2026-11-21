import { Anchor, Button, Group, Select, Stack, Table, Tabs, Text, TextInput, Title } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconApi, IconFileCode, IconHistory, IconListDetails, IconRoute, IconSearch, IconSitemap, IconTrash, IconUpload } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { DataTable, DetailList, KindBadge, MethodBadge, PageHeader, PathText, SectionCard, StatStrip, Time } from '../components/common';
import { ImportModal } from '../components/ImportModal';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { pretty } from '../lib/format';
import { methodMix } from '../lib/screens';

const KINDS = [
  { value: 'REST', label: 'REST' },
  { value: 'SOAP', label: 'SOAP' },
  { value: 'GRAPHQL', label: 'GraphQL' },
  { value: 'EVENT', label: 'Event' },
  { value: 'GRPC', label: 'gRPC' },
  { value: 'GENERIC', label: 'Generic' },
];

/** Whether an operation's name only repeats its method and path (REST imports), so the name need not be shown. */
function sameAsRequest(o: { name: string; method?: string | null; path?: string | null }): boolean {
  const n = o.name.trim();
  return n === `${o.method ?? ''} ${o.path ?? ''}`.trim() || (!!o.path && (n === o.path || `/${n}` === o.path));
}

function ServiceDetail({ id }: { id: string }) {
  const canEdit = useCan()('stubs:write', null);
  const navigate = useNavigate();
  const q = useQuery({ queryKey: ['services', id], queryFn: () => unwrap(client.GET('/__admin/services/{id}', { params: { path: { id } } })) });
  const spec = useQuery({
    queryKey: ['services', id, 'spec'],
    queryFn: () => unwrap(client.GET('/__admin/services/{id}/spec', { params: { path: { id } } })),
    enabled: !!q.data?.hasSpec,
    retry: false,
  });
  const del = useApiMutation<void, { deletedStubs: number }>({
    fn: () => unwrap(client.DELETE('/__admin/services/{id}', { params: { path: { id } } })),
    invalidate: [['services'], ['stubs'], ['info']],
    success: (d) => `Service deleted with ${d.deletedStubs} stubs`,
    onSuccess: () => void navigate('/services'),
  });
  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const s = q.data;
  if (!s) return null;
  const doc = spec.data && typeof spec.data === 'object' && 'document' in spec.data ? spec.data.document : spec.data;
  const docText = typeof doc === 'string' ? doc : pretty(doc);
  const stubCount = s.operations.reduce((n, o) => n + (o.stubIds?.length ?? 0), 0);
  const uncovered = s.operations.filter((o) => !o.stubIds?.length).length;
  const labels = Object.entries(s.labels ?? {});
  return (
    <Stack gap="md">
      <PageHeader
        title={s.name}
        breadcrumbs={[{ label: 'Services', to: '/services' }, { label: `${s.name} ${s.version}` }]}
        icon={<IconSitemap size={22} stroke={1.6} />}
        meta={
          <>
            <KindBadge kind={s.type} />
            <span className="fm-chip">v{s.version}</span>
            <Text size="sm" c="dimmed" component="span">
              from {s.sourceFormat} · imported by {s.importedBy} <Time at={s.importedAt} />
            </Text>
          </>
        }
        actions={
          <>
            {s.tag && (
              <Button component={Link} to={`/stubs?tag=${encodeURIComponent(s.tag)}`} variant="default" leftSection={<IconRoute size={16} />}>
                Stubs tagged {s.tag}
              </Button>
            )}
            {canEdit && (
              <Button
                color="red"
                variant="light"
                leftSection={<IconTrash size={16} />}
                onClick={() => confirmAction({ title: `Delete ${s.name} ${s.version}?`, message: 'The service, its stored specification and its stubs are deleted.', confirm: 'Delete service', onConfirm: () => del.mutate() })}
              >
                Delete
              </Button>
            )}
          </>
        }
      />
      <StatStrip
        label="Service summary"
        items={[
          { label: 'Operations', value: s.operations.length, hint: `from ${s.sourceFormat}` },
          { label: 'Stubs', value: stubCount, hint: 'serving its operations' },
          { label: 'Without stubs', value: uncovered, tone: uncovered ? 'warning' : 'success', hint: uncovered ? 'operations answer nothing' : 'every operation is served' },
          { label: 'Specification', value: s.hasSpec ? 'Stored' : 'None', tone: s.hasSpec ? 'success' : 'neutral', hint: s.hasSpec ? 'usable for contract tests' : 'not kept for this format' },
        ]}
      />
      {labels.length > 0 && (
        <Group gap={6} aria-label="Labels">
          {labels.map(([k, v]) => (
            <span key={k} className="fm-chip fm-chip-soft">
              {k}={v}
            </span>
          ))}
        </Group>
      )}
      <Tabs defaultValue="operations" variant="pills" radius="md" classNames={{ list: 'fm-tabs-list' }}>
        <Tabs.List>
          <Tabs.Tab value="operations" leftSection={<IconListDetails size={16} />}>
            Operations <span className="fm-tab-count">{s.operations.length}</span>
          </Tabs.Tab>
          {s.hasSpec && (
            <Tabs.Tab value="spec" leftSection={<IconFileCode size={16} />}>
              Specification
            </Tabs.Tab>
          )}
          {!!s.artifacts?.length && (
            <Tabs.Tab value="artifacts" leftSection={<IconHistory size={16} />}>
              Artifacts <span className="fm-tab-count">{s.artifacts.length}</span>
            </Tabs.Tab>
          )}
        </Tabs.List>
        <Tabs.Panel value="operations" pt="md">
          {!s.operations.length ? (
            <EmptyState title="No operations" icon={<IconListDetails size={28} />}>
              The imported document describes no operations.
            </EmptyState>
          ) : (
            <DataTable testId="service-operations" miw={640}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Operation</Table.Th>
                  <Table.Th>Stubs</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {s.operations.map((o) => (
                  <Table.Tr key={o.name}>
                    <Table.Td>
                      <Stack gap={4}>
                        {(o.method || o.path) && (
                          <Group gap={8} wrap="nowrap">
                            {o.method && <MethodBadge method={o.method} />}
                            {o.path && <PathText path={o.path} kind="template" />}
                          </Group>
                        )}
                        {!sameAsRequest(o) && (
                          <Text size="xs" c="dimmed" className="mono break-all">
                            {o.name}
                          </Text>
                        )}
                      </Stack>
                    </Table.Td>
                    <Table.Td data-label="Stubs">
                      {o.stubIds?.length ? (
                        <Group gap={4}>
                          {o.stubIds.map((sid, i) => (
                            <Anchor key={sid} component={Link} to={`/stubs/${sid}`} className="fm-chip" underline="never" aria-label={`Stub ${String(i + 1)} of ${o.name}`}>
                              #{i + 1}
                            </Anchor>
                          ))}
                        </Group>
                      ) : (
                        <span className="fm-status" data-tone="warning">
                          no stub
                        </span>
                      )}
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </DataTable>
          )}
        </Tabs.Panel>
        <Tabs.Panel value="spec" pt="md">
          {spec.isLoading ? <LoadingState /> : spec.isError ? <ErrorState error={spec.error} /> : <CodeEditor value={docText} readOnly language={docText.trimStart().startsWith('<') ? 'xml' : 'json'} minHeight={300} maxHeight={800} label="Stored specification" />}
        </Tabs.Panel>
        <Tabs.Panel value="artifacts" pt="md">
          <DataTable>
            <Table.Thead>
              <Table.Tr>
                <Table.Th>Artifact</Table.Th>
                <Table.Th>Imported by</Table.Th>
                <Table.Th>When</Table.Th>
              </Table.Tr>
            </Table.Thead>
            <Table.Tbody>
              {(s.artifacts ?? []).map((a, i) => (
                <Table.Tr key={i}>
                  <Table.Td>
                    <span className="fm-chip">{a.kind}</span>
                  </Table.Td>
                  <Table.Td data-label="Imported by">{a.importedBy}</Table.Td>
                  <Table.Td data-label="When">
                    <Time at={a.importedAt} />
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </DataTable>
        </Tabs.Panel>
      </Tabs>
      {s.importOptions && Object.keys(s.importOptions).length > 0 && (
        <SectionCard title="Import options" description="Applied again when the service is re-imported" icon={<IconUpload size={16} />}>
          <DetailList
            items={Object.entries(s.importOptions).map(([k, v]) => ({
              label: k,
              value:
                v === '' || v == null ? (
                  <Text size="sm" c="dimmed" component="span">
                    none
                  </Text>
                ) : (
                  <span className="mono">{typeof v === 'string' ? v : JSON.stringify(v)}</span>
                ),
            }))}
          />
        </SectionCard>
      )}
    </Stack>
  );
}

function ServiceList() {
  const can = useCan();
  const canImport = can('imports:run');
  const [kind, setKind] = useState<string | null>(null);
  const [name, setName] = useState('');
  const [label, setLabel] = useState('');
  const [importOpen, setImportOpen] = useState(false);
  const [dName] = useDebouncedValue(name, 300);
  const [dLabel] = useDebouncedValue(label, 300);
  const query = { type: kind ?? undefined, q: dName || undefined, label: dLabel || undefined };
  const q = useQuery({ queryKey: ['services', 'list', query], queryFn: () => unwrap(client.GET('/__admin/services', { params: { query } })) });
  const filtered = !!(kind || dName || dLabel);
  const services = q.data?.services ?? [];
  return (
    <Stack gap="md">
      <PageHeader
        title="Services"
        description="APIs imported from OpenAPI, AsyncAPI, GraphQL, WSDL/SoapUI, Postman, gRPC or Microcks artifacts, with their operations and the stubs that serve them."
        actions={
          canImport && (
            <Button leftSection={<IconUpload size={16} />} onClick={() => setImportOpen(true)}>
              Import an API
            </Button>
          )
        }
      />
      <div className="fm-toolbar">
        <TextInput placeholder="Name contains" leftSection={<IconSearch size={16} />} value={name} onChange={(e) => setName(e.currentTarget.value)} aria-label="Name" className="fm-grow" />
        <Select placeholder="Any type" data={KINDS} value={kind} onChange={setKind} clearable aria-label="Type" w={160} />
        <TextInput placeholder="label or key=value" value={label} onChange={(e) => setLabel(e.currentTarget.value)} aria-label="Label" w={220} classNames={{ input: 'mono' }} />
      </div>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !services.length ? (
        filtered ? (
          <EmptyState
            title="No services match the filters"
            icon={<IconSearch size={28} />}
            action={
              <Button
                variant="default"
                onClick={() => {
                  setName('');
                  setLabel('');
                  setKind(null);
                }}
              >
                Clear filters
              </Button>
            }
          >
            Try another name, type or label.
          </EmptyState>
        ) : (
          <EmptyState
            title="No services yet"
            icon={<IconSitemap size={30} />}
            docs={{ label: 'How the service catalog works', href: docsUrl('guide/service-catalog') }}
            action={
              canImport ? (
                <Button leftSection={<IconUpload size={16} />} onClick={() => setImportOpen(true)}>
                  Import an API description
                </Button>
              ) : undefined
            }
          >
            Every import of an OpenAPI, AsyncAPI, GraphQL, WSDL or Postman document and every gRPC descriptor set records a service here, with its operations and stubs.
          </EmptyState>
        )
      ) : (
        <>
          <div className="fm-card-grid" data-testid="services-grid">
            {services.map((s) => {
              const mix = methodMix(s.operations);
              const labels = Object.entries(s.labels ?? {});
              return (
                <Link key={s.id} to={`/services/${encodeURIComponent(s.id)}`} className="fm-card-link" data-testid="service-card">
                  <Group justify="space-between" wrap="nowrap" align="flex-start" gap="sm">
                    <Group gap="sm" wrap="nowrap" miw={0}>
                      <span className="fm-section-icon" aria-hidden>
                        <IconApi size={18} />
                      </span>
                      <Stack gap={0} miw={0}>
                        <Title order={3} size="h5" className="truncate">
                          {s.name}
                        </Title>
                        <Text size="xs" c="dimmed" className="truncate">
                          v{s.version} · {s.sourceFormat}
                        </Text>
                      </Stack>
                    </Group>
                    <KindBadge kind={s.type} />
                  </Group>
                  <Group gap={6} wrap="wrap">
                    <Text size="sm" fw={700} className="tabular" component="span">
                      {s.operations.length}
                    </Text>
                    <Text size="sm" c="dimmed" component="span">
                      operation{s.operations.length === 1 ? '' : 's'}
                    </Text>
                    {mix.map(([m, n]) => (
                      <span key={m} className="fm-chip" title={`${String(n)} ${m}`}>
                        {m} {n}
                      </span>
                    ))}
                  </Group>
                  {labels.length > 0 && (
                    <Group gap={4} wrap="wrap">
                      {labels.slice(0, 3).map(([k, v]) => (
                        <span key={k} className="fm-chip fm-chip-soft">
                          {k}={v}
                        </span>
                      ))}
                      {labels.length > 3 && (
                        <Text size="xs" c="dimmed">
                          +{labels.length - 3}
                        </Text>
                      )}
                    </Group>
                  )}
                  <Text size="xs" c="dimmed" mt="auto">
                    imported <Time at={s.importedAt} /> by {s.importedBy}
                  </Text>
                </Link>
              );
            })}
          </div>
          <Text size="sm" c="dimmed">
            {services.length} service{services.length === 1 ? '' : 's'}
          </Text>
        </>
      )}
      <ImportModal opened={importOpen} onClose={() => setImportOpen(false)} />
    </Stack>
  );
}

export default function Services() {
  const { id } = useParams();
  return (
    <FeatureGate path="/__admin/services" method="get" what="The services catalog">
      {id ? <ServiceDetail id={id} /> : <ServiceList />}
    </FeatureGate>
  );
}
