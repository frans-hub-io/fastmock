import { ActionIcon, Button, Group, Paper, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { Dropzone } from '@mantine/dropzone';
import { IconDownload, IconTrash, IconUpload } from '@tabler/icons-react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { errorFromResponse, rawFetch, api } from '../api/http';
import { confirmAction, notifyError, notifySuccess, useApiMutation } from '../api/mutations';
import { useCan, useSpace, useSpaceChoice, useSpaceQuery } from '../auth/useAuth';
import { Permitted } from '../components/Permitted';
import { SpaceBadge, SpaceField } from '../components/spaces';
import { PageHeader, Time } from '../components/common';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { downloadBlob } from '../lib/download';
import { formatBytes } from '../lib/format';

interface FileInfo {
  name: string;
  contentType: string;
  size: number;
  updatedAt?: string | null;
  space?: string | null;
}

const MAX = 10 * 1024 * 1024;

function at(space: string | null | undefined): { space?: string } {
  return space ? { space } : {};
}

function filePath(name: string): string {
  return `/__admin/files/${name.split('/').map(encodeURIComponent).join('/')}`;
}

function FilesInner() {
  const can = useCan();
  const canEdit = can('files:write');
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const qc = useQueryClient();
  const [prefix, setPrefix] = useState('');
  const [target, setTarget] = useSpaceChoice('files:write');
  const [busy, setBusy] = useState(false);
  const q = useQuery({ queryKey: ['files', spaceQ], queryFn: () => api.get<{ total: number; files: FileInfo[] }>('/__admin/files', { query: spaceQ }) });
  const del = useApiMutation<FileInfo>({ fn: (f) => api.del(filePath(f.name), { query: at(f.space) }), invalidate: [['files']], success: (_d, f) => `Deleted ${f.name}` });

  const upload = async (files: File[]) => {
    setBusy(true);
    try {
      for (const f of files) {
        const name = `${prefix ? `${prefix.replace(/\/+$/, '')}/` : ''}${f.name}`;
        const res = await rawFetch('PUT', filePath(name), { rawBody: f, contentType: f.type || 'application/octet-stream', query: at(target) });
        if (!res.ok) throw await errorFromResponse(res);
      }
      notifySuccess(`Uploaded ${files.length} file${files.length === 1 ? '' : 's'}`);
      await qc.invalidateQueries({ queryKey: ['files'] });
    } catch (e) {
      notifyError(e, 'Upload failed');
    } finally {
      setBusy(false);
    }
  };

  const download = async (f: FileInfo) => {
    try {
      const res = await rawFetch('GET', filePath(f.name), { accept: '*/*', query: at(f.space) });
      if (!res.ok) throw await errorFromResponse(res);
      downloadBlob(f.name.split('/').pop() ?? 'file', await res.blob());
    } catch (e) {
      notifyError(e, 'Download failed');
    }
  };

  return (
    <Stack gap="md">
      <PageHeader title="Files" description="Body files that stubs reference with response.bodyFileName. At most 10 MiB each." />
      {canEdit && (
        <Paper withBorder p="md" radius="md">
          <Group gap="sm" mb="sm" align="flex-end">
            <SpaceField perm="files:write" value={target} onChange={setTarget} />
            <TextInput label="Folder (optional)" placeholder="payments/v1" value={prefix} onChange={(e) => setPrefix(e.currentTarget.value)} w={260} />
          </Group>
          <Dropzone onDrop={(f) => void upload(f)} maxSize={MAX} loading={busy} onReject={(r) => notifyError(new Error(r[0]?.errors[0]?.message ?? 'rejected'), 'File rejected')}>
            <Group justify="center" mih={80} style={{ pointerEvents: 'none' }}>
              <IconUpload size={28} />
              <Text>Drop files to upload, or click to choose</Text>
            </Group>
          </Dropzone>
        </Paper>
      )}
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !q.data?.files.length ? (
        <EmptyState title="No files" />
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm" highlightOnHover>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Name</Table.Th>
                  <Table.Th>Type</Table.Th>
                  <Table.Th>Size</Table.Th>
                  <Table.Th>Updated</Table.Th>
                  {spaces.enabled && <Table.Th>Space</Table.Th>}
                  <Table.Th />
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {q.data.files.map((f) => (
                  <Table.Tr key={`${f.space ?? ''}/${f.name}`}>
                    <Table.Td className="mono">{f.name}</Table.Td>
                    <Table.Td>{f.contentType}</Table.Td>
                    <Table.Td>{formatBytes(f.size)}</Table.Td>
                    <Table.Td>
                      <Time at={f.updatedAt} />
                    </Table.Td>
                    {spaces.enabled && (
                      <Table.Td>
                        <SpaceBadge space={f.space} />
                      </Table.Td>
                    )}
                    <Table.Td>
                      <Group gap={4} wrap="nowrap" justify="flex-end">
                        <Tooltip label="Download">
                          <ActionIcon variant="subtle" aria-label={`Download ${f.name}`} onClick={() => void download(f)}>
                            <IconDownload size={16} />
                          </ActionIcon>
                        </Tooltip>
                        {can('files:write', f.space ?? null) && (
                          <Tooltip label="Delete">
                            <ActionIcon
                              variant="subtle"
                              color="red"
                              aria-label={`Delete ${f.name}`}
                              onClick={() => confirmAction({ title: 'Delete file?', message: `Stubs that use ${f.name} will fail to render.`, confirm: 'Delete', onConfirm: () => del.mutate(f) })}
                            >
                              <IconTrash size={16} />
                            </ActionIcon>
                          </Tooltip>
                        )}
                      </Group>
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
      {!canEdit && (
        <Group>
          <Permitted perm="files:write">
            <Button variant="default" leftSection={<IconUpload size={16} />}>
              Upload files
            </Button>
          </Permitted>
        </Group>
      )}
    </Stack>
  );
}

export default function Files() {
  return (
    <FeatureGate path="/__admin/files" method="get" what="File storage">
      <FilesInner />
    </FeatureGate>
  );
}
