import { ActionIcon, Badge, Button, FileButton, Group, Paper, Stack, Table, Text, TextInput } from '@mantine/core';
import { IconTrash, IconUpload } from '@tabler/icons-react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { errorFromResponse, rawFetch } from '../api/http';
import { confirmAction, notifyError, notifySuccess, useApiMutation } from '../api/mutations';
import { useCan } from '../auth/useAuth';
import { PageHeader } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';

export default function Grpc() {
  const canEdit = useCan('editor');
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

  return (
    <Stack gap="md">
      <PageHeader
        title="gRPC descriptors"
        description={
          <>
            Compiled protobuf FileDescriptorSets (<code>protoc --include_imports --descriptor_set_out=api.pb</code>). Stubs match gRPC calls by
            /package.Service/Method.
          </>
        }
      />
      {canEdit && (
        <Paper withBorder p="md" radius="md">
          <Group gap="sm" align="flex-end">
            <TextInput label="Name" placeholder="defaults to the file name" value={name} onChange={(e) => setName(e.currentTarget.value)} w={260} />
            <FileButton onChange={(f) => void upload(f)} accept=".pb,.desc,.protoset,.bin,application/octet-stream">
              {(props) => (
                <Button {...props} leftSection={<IconUpload size={16} />} loading={busy}>
                  Upload descriptor set
                </Button>
              )}
            </FileButton>
          </Group>
          <Text size="xs" c="dimmed" mt={6}>
            Up to 16 MiB. A new version must stay compatible with the services already in use.
          </Text>
        </Paper>
      )}
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : (
        <>
          {!q.data?.descriptors.length ? (
            <EmptyState title="No descriptor sets" />
          ) : (
            <Paper withBorder radius="md">
              <Table fz="sm">
                <Table.Thead>
                  <Table.Tr>
                    <Table.Th>Name</Table.Th>
                    <Table.Th>Services</Table.Th>
                    <Table.Th />
                  </Table.Tr>
                </Table.Thead>
                <Table.Tbody>
                  {q.data.descriptors.map((d) => (
                    <Table.Tr key={d.name}>
                      <Table.Td className="mono">{d.name}</Table.Td>
                      <Table.Td>
                        <Group gap={4}>
                          {d.services.map((s) => (
                            <Badge key={s} variant="light" style={{ textTransform: 'none' }}>
                              {s}
                            </Badge>
                          ))}
                        </Group>
                      </Table.Td>
                      <Table.Td>
                        {canEdit && (
                          <ActionIcon variant="subtle" color="red" aria-label={`Delete ${d.name}`} onClick={() => confirmAction({ title: 'Remove descriptor set?', message: `gRPC stubs using ${d.name} stop matching.`, confirm: 'Remove', onConfirm: () => del.mutate(d.name) })}>
                            <IconTrash size={16} />
                          </ActionIcon>
                        )}
                      </Table.Td>
                    </Table.Tr>
                  ))}
                </Table.Tbody>
              </Table>
            </Paper>
          )}
          <Text size="sm" c="dimmed">
            Loaded services: {q.data?.loadedServices.length ? q.data.loadedServices.join(', ') : 'none'}
          </Text>
        </>
      )}
    </Stack>
  );
}
