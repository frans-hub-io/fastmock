import { ActionIcon, Button, Group, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { Dropzone } from '@mantine/dropzone';
import { IconBraces, IconCode, IconDownload, IconFile, IconFileText, IconFiles, IconPhoto, IconSearch, IconTrash, IconUpload } from '@tabler/icons-react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { errorFromResponse, rawFetch, api } from '../api/http';
import { confirmAction, notifyError, notifySuccess, useApiMutation } from '../api/mutations';
import { useCan, useSpace, useSpaceChoice, useSpaceQuery } from '../auth/useAuth';
import { Permitted } from '../components/Permitted';
import { SpaceBadge, SpaceField } from '../components/spaces';
import { Copy, DataTable, DensityToggle, PageHeader, SectionCard, Time } from '../components/common';
import { EmptyState, ErrorState, FeatureGate, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { downloadBlob } from '../lib/download';
import { formatBytes } from '../lib/format';
import { fileKind, folderError, splitName, type FileKind } from '../lib/screens';
import { useDensity } from '../lib/ui';

interface FileInfo {
  name: string;
  contentType: string;
  size: number;
  updatedAt?: string | null;
  space?: string | null;
}

const MAX = 10 * 1024 * 1024;

const KIND_ICON: Record<FileKind, typeof IconFile> = { json: IconBraces, xml: IconCode, text: IconFileText, image: IconPhoto, binary: IconFile };

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
  const [filter, setFilter] = useState('');
  const [target, setTarget] = useSpaceChoice('files:write');
  const [busy, setBusy] = useState(false);
  const [density, setDensity] = useDensity();
  const q = useQuery({ queryKey: ['files', spaceQ], queryFn: () => api.get<{ total: number; files: FileInfo[] }>('/__admin/files', { query: spaceQ }) });
  const del = useApiMutation<FileInfo>({ fn: (f) => api.del(filePath(f.name), { query: at(f.space) }), invalidate: [['files']], success: (_d, f) => `Deleted ${f.name}` });
  const badFolder = folderError(prefix);

  const upload = async (files: File[]) => {
    if (badFolder) return;
    setBusy(true);
    try {
      for (const f of files) {
        const name = `${prefix.trim() ? `${prefix.trim().replace(/\/+$/, '')}/` : ''}${f.name}`;
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

  const all = q.data?.files ?? [];
  const files = all.filter((f) => !filter || f.name.toLowerCase().includes(filter.toLowerCase()));
  const totalBytes = all.reduce((n, f) => n + f.size, 0);

  return (
    <Stack gap="md">
      <PageHeader
        title="Files"
        description={
          <>
            Body files that stubs reference with <span className="mono">response.bodyFileName</span>. At most 10 MiB each.
          </>
        }
        meta={
          all.length > 0 && (
            <>
              <span className="fm-chip">
                {all.length} file{all.length === 1 ? '' : 's'}
              </span>
              <span className="fm-chip">{formatBytes(totalBytes)}</span>
            </>
          )
        }
        actions={
          !canEdit && (
            <Permitted perm="files:write">
              <Button variant="default" leftSection={<IconUpload size={16} />}>
                Upload files
              </Button>
            </Permitted>
          )
        }
      />
      {canEdit && (
        <SectionCard title="Upload" description="Files keep their name; a folder becomes part of the name stubs refer to" icon={<IconUpload size={16} />}>
          <Stack gap="sm">
            <Group gap="sm" align="flex-start" wrap="wrap">
              <SpaceField perm="files:write" value={target} onChange={setTarget} />
              <TextInput
                label="Folder (optional)"
                placeholder="payments/v1"
                value={prefix}
                onChange={(e) => setPrefix(e.currentTarget.value)}
                error={badFolder}
                classNames={{ input: 'mono' }}
                w={{ base: '100%', sm: 280 }}
              />
            </Group>
            <Dropzone
              onDrop={(f) => void upload(f)}
              maxSize={MAX}
              loading={busy}
              disabled={!!badFolder}
              className="fm-dropzone"
              onReject={(r) => notifyError(new Error(r[0]?.errors[0]?.message ?? 'rejected'), 'File rejected')}
              data-testid="files-dropzone"
            >
              <Group justify="center" gap="md" mih={96} className="fm-dropzone-inner" wrap="nowrap">
                <span className="fm-section-icon" aria-hidden>
                  <IconUpload size={18} />
                </span>
                <div>
                  <Text fw={600}>Drop files to upload, or click to choose</Text>
                  <Text size="xs" c="dimmed">
                    Stored as {prefix.trim() ? <span className="mono">{prefix.trim().replace(/\/+$/, '')}/</span> : null}
                    <span className="mono">&lt;file name&gt;</span> · up to 10 MiB each
                  </Text>
                </div>
              </Group>
            </Dropzone>
          </Stack>
        </SectionCard>
      )}
      {all.length > 0 && (
        <div className="fm-toolbar">
          <TextInput placeholder="Filter by name" leftSection={<IconSearch size={16} />} value={filter} onChange={(e) => setFilter(e.currentTarget.value)} aria-label="Filter files" className="fm-grow" />
          <DensityToggle value={density} onChange={setDensity} />
        </div>
      )}
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !all.length ? (
        <EmptyState title="No files yet" icon={<IconFiles size={30} />} docs={{ label: 'Body files in responses', href: docsUrl('guide/responses') }}>
          {canEdit ? 'Upload JSON, XML or any other body above, then point a stub at it with response.bodyFileName.' : 'Body files appear here once an editor uploads them.'}
        </EmptyState>
      ) : !files.length ? (
        <EmptyState
          title="No files match the filter"
          icon={<IconSearch size={28} />}
          action={
            <Button variant="default" onClick={() => setFilter('')}>
              Clear filter
            </Button>
          }
        >
          Try another part of the name.
        </EmptyState>
      ) : (
        <DataTable
          testId="files-table"
          density={density}
          miw={820}
          footer={
            <Text size="xs" c="dimmed">
              {files.length} of {all.length} file{all.length === 1 ? '' : 's'} · {formatBytes(files.reduce((n, f) => n + f.size, 0))}
            </Text>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Name</Table.Th>
              <Table.Th>Type</Table.Th>
              <Table.Th ta="right">Size</Table.Th>
              <Table.Th>Updated</Table.Th>
              {spaces.enabled && <Table.Th>Space</Table.Th>}
              <Table.Th w={120}>
                <span className="sr-only">Actions</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {files.map((f) => {
              const { dir, base } = splitName(f.name);
              const Icon = KIND_ICON[fileKind(f.name, f.contentType)];
              return (
                <Table.Tr key={`${f.space ?? ''}/${f.name}`} data-testid="file-row">
                  <Table.Td>
                    <Group gap="sm" wrap="nowrap">
                      <span className="fm-section-icon" aria-hidden>
                        <Icon size={16} />
                      </span>
                      <Text size="sm" className="mono break-all" component="span">
                        {dir && <span className="fm-path-query">{dir}</span>}
                        <strong>{base}</strong>
                      </Text>
                    </Group>
                  </Table.Td>
                  <Table.Td data-label="Type">
                    <span className="fm-chip">{f.contentType}</span>
                  </Table.Td>
                  <Table.Td data-label="Size" ta="right" className="tabular fm-nowrap">
                    {formatBytes(f.size)}
                  </Table.Td>
                  <Table.Td data-label="Updated">
                    <Time at={f.updatedAt} />
                  </Table.Td>
                  {spaces.enabled && (
                    <Table.Td data-label="Space">
                      <SpaceBadge space={f.space} />
                    </Table.Td>
                  )}
                  <Table.Td>
                    <Group gap={2} wrap="nowrap" justify="flex-end">
                      <Copy value={f.name} label={`Copy the bodyFileName of ${f.name}`} />
                      <Tooltip label="Download">
                        <ActionIcon variant="subtle" color="gray" aria-label={`Download ${f.name}`} onClick={() => void download(f)}>
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
              );
            })}
          </Table.Tbody>
        </DataTable>
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
