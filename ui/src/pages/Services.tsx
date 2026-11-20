import { Anchor, Badge, Button, Card, Group, Paper, Select, Stack, Table, Tabs, Text, TextInput, Title } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconArrowLeft, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link, useNavigate, useParams } from 'react-router';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { MethodBadge, PageHeader, Time } from '../components/common';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { pretty } from '../lib/format';

const KIND_COLOR: Record<string, string> = { REST: 'blue', SOAP: 'orange', GRAPHQL: 'pink', EVENT: 'teal', GRPC: 'violet', GENERIC: 'gray' };
const KINDS = ['REST', 'SOAP', 'GRAPHQL', 'EVENT', 'GRPC', 'GENERIC'];

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
  return (
    <Stack gap="md">
      <Anchor component={Link} to="/services" size="sm">
        <Group gap={4}>
          <IconArrowLeft size={14} /> All services
        </Group>
      </Anchor>
      <PageHeader
        title={`${s.name} ${s.version}`}
        description={
          <Group gap="xs">
            <Badge color={KIND_COLOR[s.type] ?? 'gray'}>{s.type}</Badge>
            <Text size="sm">from {s.sourceFormat}</Text>
            <Text size="sm">
              · imported by {s.importedBy} <Time at={s.importedAt} />
            </Text>
            {s.tag && (
              <Anchor component={Link} to={`/stubs?tag=${encodeURIComponent(s.tag)}`} size="sm">
                stubs tagged {s.tag}
              </Anchor>
            )}
          </Group>
        }
        actions={
          canEdit && (
            <Button
              color="red"
              variant="light"
              leftSection={<IconTrash size={16} />}
              onClick={() => confirmAction({ title: `Delete ${s.name} ${s.version}?`, message: 'The service, its stored specification and its stubs are deleted.', confirm: 'Delete service', onConfirm: () => del.mutate() })}
            >
              Delete
            </Button>
          )
        }
      />
      {Object.keys(s.labels ?? {}).length > 0 && (
        <Group gap={6}>
          {Object.entries(s.labels ?? {}).map(([k, v]) => (
            <Badge key={k} variant="outline" style={{ textTransform: 'none' }}>
              {k}={v}
            </Badge>
          ))}
        </Group>
      )}
      <Tabs defaultValue="operations">
        <Tabs.List>
          <Tabs.Tab value="operations">Operations ({s.operations.length})</Tabs.Tab>
          {s.hasSpec && <Tabs.Tab value="spec">Specification</Tabs.Tab>}
          {!!s.artifacts?.length && <Tabs.Tab value="artifacts">Artifacts</Tabs.Tab>}
        </Tabs.List>
        <Tabs.Panel value="operations" pt="md">
          {!s.operations.length ? (
            <EmptyState title="No operations" />
          ) : (
            <Paper withBorder radius="md">
              <div className="table-scroll">
                <Table fz="sm">
                  <Table.Thead>
                    <Table.Tr>
                      <Table.Th>Operation</Table.Th>
                      <Table.Th>Method</Table.Th>
                      <Table.Th>Path</Table.Th>
                      <Table.Th>Stubs</Table.Th>
                    </Table.Tr>
                  </Table.Thead>
                  <Table.Tbody>
                    {s.operations.map((o) => (
                      <Table.Tr key={o.name}>
                        <Table.Td fw={500}>{o.name}</Table.Td>
                        <Table.Td>{o.method ? <MethodBadge method={o.method} /> : '—'}</Table.Td>
                        <Table.Td className="mono">{o.path ?? '—'}</Table.Td>
                        <Table.Td>
                          <Group gap={4}>
                            {(o.stubIds ?? []).map((sid, i) => (
                              <Anchor key={sid} component={Link} to={`/stubs/${sid}`} size="sm">
                                #{i + 1}
                              </Anchor>
                            ))}
                            {!o.stubIds?.length && '—'}
                          </Group>
                        </Table.Td>
                      </Table.Tr>
                    ))}
                  </Table.Tbody>
                </Table>
              </div>
            </Paper>
          )}
        </Tabs.Panel>
        <Tabs.Panel value="spec" pt="md">
          {spec.isLoading ? <LoadingState /> : spec.isError ? <ErrorState error={spec.error} /> : <CodeEditor value={docText} readOnly language={docText.trimStart().startsWith('<') ? 'xml' : 'json'} minHeight={300} maxHeight={800} label="Stored specification" />}
        </Tabs.Panel>
        <Tabs.Panel value="artifacts" pt="md">
          <Table fz="sm">
            <Table.Tbody>
              {(s.artifacts ?? []).map((a, i) => (
                <Table.Tr key={i}>
                  <Table.Td>{a.kind}</Table.Td>
                  <Table.Td>{a.importedBy}</Table.Td>
                  <Table.Td>
                    <Time at={a.importedAt} />
                  </Table.Td>
                </Table.Tr>
              ))}
            </Table.Tbody>
          </Table>
        </Tabs.Panel>
      </Tabs>
    </Stack>
  );
}

function ServiceList() {
  const [kind, setKind] = useState<string | null>(null);
  const [name, setName] = useState('');
  const [label, setLabel] = useState('');
  const [dName] = useDebouncedValue(name, 300);
  const [dLabel] = useDebouncedValue(label, 300);
  const query = { type: kind ?? undefined, q: dName || undefined, label: dLabel || undefined };
  const q = useQuery({ queryKey: ['services', 'list', query], queryFn: () => unwrap(client.GET('/__admin/services', { params: { query } })) });
  return (
    <Stack gap="md">
      <PageHeader title="Services" description="APIs imported from OpenAPI, AsyncAPI, GraphQL, WSDL/SoapUI, Postman, gRPC or Microcks artifacts, with their operations and the stubs that serve them." />
      <Paper withBorder p="sm" radius="md">
        <Group gap="sm" wrap="wrap">
          <TextInput placeholder="Name contains" value={name} onChange={(e) => setName(e.currentTarget.value)} aria-label="Name" />
          <Select placeholder="Any type" data={KINDS} value={kind} onChange={setKind} clearable aria-label="Type" w={150} />
          <TextInput placeholder="label or key=value" value={label} onChange={(e) => setLabel(e.currentTarget.value)} aria-label="Label" />
        </Group>
      </Paper>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data?.services.length ? (
        <EmptyState title="No services">Import an API description from the Stubs page to create one.</EmptyState>
      ) : (
        <div style={{ display: 'grid', gridTemplateColumns: 'repeat(auto-fill, minmax(280px, 1fr))', gap: 'var(--mantine-spacing-md)' }}>
          {q.data.services.map((s) => (
            <Card key={s.id} withBorder radius="md" component={Link} to={`/services/${encodeURIComponent(s.id)}`} style={{ textDecoration: 'none' }}>
              <Group justify="space-between" mb={6} wrap="nowrap">
                <Title order={3} size="h5" className="truncate">
                  {s.name}
                </Title>
                <Badge color={KIND_COLOR[s.type] ?? 'gray'}>{s.type}</Badge>
              </Group>
              <Text size="sm" c="dimmed">
                version {s.version} · {s.operations.length} operations · {s.sourceFormat}
              </Text>
              <Text size="xs" c="dimmed" mt={6}>
                imported <Time at={s.importedAt} /> by {s.importedBy}
              </Text>
            </Card>
          ))}
        </div>
      )}
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
