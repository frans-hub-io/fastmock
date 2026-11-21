import { ActionIcon, Button, FileButton, Group, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { IconFileCode, IconTrash, IconUpload } from '@tabler/icons-react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { errorFromResponse, rawFetch } from '../api/http';
import { confirmAction, notifyError, notifySuccess, useApiMutation } from '../api/mutations';
import { useCan } from '../auth/useAuth';
import { CommandSnippet, DataTable, PageHeader, SectionCard } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';

const PROTOC = 'protoc --include_imports --descriptor_set_out=api.pb -I proto proto/*.proto';

/** `package.Service` split for display: the package dimmed, the service name strong. */
function ServiceName({ full }: { full: string }) {
  const i = full.lastIndexOf('.');
  return (
    <span className="fm-chip" title={full}>
      {i > 0 && <span className="fm-path-query">{full.slice(0, i + 1)}</span>}
      <strong>{i > 0 ? full.slice(i + 1) : full}</strong>
    </span>
  );
}

export default function Grpc() {
  const can = useCan();
  const canEdit = can('grpc:write');
  const qc = useQueryClient();
  const [name, setName] = useState('');
  const [busy, setBusy] = useState(false);
  const q = useQuery({ queryKey: ['grpc'], queryFn: () => unwrap(client.GET('/__admin/grpc/descriptors')) });
  const del = useApiMutation<string>({ fn: (n) => unwrap(client.DELETE('/__admin/grpc/descriptors/{name}', { params: { path: { name: n } } })), invalidate: [['grpc']], success: (_d, n) => `Removed ${n}` });

  const upload = async (file: File | null) => {
    if (!file) return;
    const n = name.trim() || file.name.replace(/\.(pb|desc|protoset|bin)$/i, '');
    setBusy(true);
    try {
      const res = await rawFetch('PUT', `/__admin/grpc/descriptors/${encodeURIComponent(n)}`, { rawBody: file, contentType: 'application/octet-stream' });
      if (!res.ok) throw await errorFromResponse(res);
      notifySuccess(`Uploaded descriptor set ${n}`);
      setName('');
      await qc.invalidateQueries({ queryKey: ['grpc'] });
    } catch (e) {
      notifyError(e, 'Upload failed');
    } finally {
      setBusy(false);
    }
  };

  const descriptors = q.data?.descriptors ?? [];
  const loaded = q.data?.loadedServices ?? [];

  return (
    <Stack gap="md">
      <PageHeader
        title="gRPC descriptors"
        description="Compiled protobuf FileDescriptorSets. Stubs match gRPC calls by /package.Service/Method."
        meta={
          q.data && (
            <>
              <span className="fm-chip">
                {descriptors.length} descriptor set{descriptors.length === 1 ? '' : 's'}
              </span>
              <span className="fm-chip fm-chip-soft">
                {loaded.length} service{loaded.length === 1 ? '' : 's'} loaded
              </span>
            </>
          )
        }
      />
      {canEdit && (
        <SectionCard title="Upload a descriptor set" description="Up to 16 MiB. A new version must stay compatible with the services already in use." icon={<IconUpload size={16} />}>
          <Stack gap="sm">
            <Group gap="sm" align="flex-start" wrap="wrap">
              <TextInput
                label="Name"
                placeholder="defaults to the file name"
                value={name}
                onChange={(e) => setName(e.currentTarget.value)}
                classNames={{ input: 'mono' }}
                w={{ base: '100%', sm: 280 }}
              />
              <FileButton onChange={(f) => void upload(f)} accept=".pb,.desc,.protoset,.bin,application/octet-stream">
                {(props) => (
                  <Button {...props} leftSection={<IconUpload size={16} />} loading={busy} mt={{ base: 0, sm: 25 }}>
                    Upload descriptor set
                  </Button>
                )}
              </FileButton>
            </Group>
            <Text size="xs" c="dimmed">
              Build one with protoc, keeping the imported files:
            </Text>
            <CommandSnippet command={PROTOC} testId="protoc-command" />
          </Stack>
        </SectionCard>
      )}
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !descriptors.length ? (
        <EmptyState title="No descriptor sets" icon={<IconFileCode size={30} />} docs={{ label: 'gRPC mocking guide', href: docsUrl('guide/grpc') }}>
          {canEdit ? 'Upload a FileDescriptorSet compiled with protoc; its services become available to gRPC stubs at once.' : 'gRPC stubs need the descriptor sets of their services. Ask an editor to upload them.'}
        </EmptyState>
      ) : (
        <DataTable testId="grpc-table" miw={640}>
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Descriptor set</Table.Th>
              <Table.Th>Services</Table.Th>
              {canEdit && (
                <Table.Th w={56}>
                  <span className="sr-only">Actions</span>
                </Table.Th>
              )}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {descriptors.map((d) => (
              <Table.Tr key={d.name} data-testid="grpc-row">
                <Table.Td>
                  <Group gap="sm" wrap="nowrap">
                    <span className="fm-section-icon" aria-hidden>
                      <IconFileCode size={16} />
                    </span>
                    <Stack gap={0} miw={0}>
                      <Text size="sm" fw={700} className="mono break-all">
                        {d.name}
                      </Text>
                      <Text size="xs" c="dimmed">
                        {d.services.length} service{d.services.length === 1 ? '' : 's'}
                      </Text>
                    </Stack>
                  </Group>
                </Table.Td>
                <Table.Td data-label="Services">
                  <Group gap={4} wrap="wrap">
                    {d.services.map((s) => (
                      <ServiceName key={s} full={s} />
                    ))}
                  </Group>
                </Table.Td>
                {canEdit && (
                  <Table.Td>
                    <Group justify="flex-end">
                      <Tooltip label="Remove descriptor set">
                        <ActionIcon variant="subtle" color="red" aria-label={`Delete ${d.name}`} onClick={() => confirmAction({ title: 'Remove descriptor set?', message: `gRPC stubs using ${d.name} stop matching.`, confirm: 'Remove', onConfirm: () => del.mutate(d.name) })}>
                          <IconTrash size={16} />
                        </ActionIcon>
                      </Tooltip>
                    </Group>
                  </Table.Td>
                )}
              </Table.Tr>
            ))}
          </Table.Tbody>
        </DataTable>
      )}
      {q.data && (
        <SectionCard title="Loaded services" description="What the gRPC endpoint serves now, from every descriptor set" icon={<IconFileCode size={16} />}>
          {loaded.length ? (
            <Stack gap={6}>
              <Group gap={6} wrap="wrap" data-testid="grpc-loaded">
                {loaded.map((s) => (
                  <ServiceName key={s} full={s} />
                ))}
              </Group>
              <Text size="xs" c="dimmed">
                A stub for <span className="mono">/{loaded[0]}/Method</span> answers calls to that method.
              </Text>
            </Stack>
          ) : (
            <Text size="sm" c="dimmed" data-testid="grpc-loaded">
              none
            </Text>
          )}
        </SectionCard>
      )}
    </Stack>
  );
}
