import { Alert, Badge, Button, Checkbox, Group, List, Modal, Stack, Text, TextInput } from '@mantine/core';
import { Dropzone } from '@mantine/dropzone';
import { IconFileImport, IconUpload, IconX } from '@tabler/icons-react';
import { useQueryClient } from '@tanstack/react-query';
import { useState } from 'react';
import { api, errorMessage } from '../api/http';
import { notifySuccess } from '../api/mutations';
import { readFileText } from '../lib/download';
import { detectImportKind, type ImportKind } from '../lib/stubs';

interface ImportResult {
  imported: number;
  removed: number;
  warnings?: string[];
  dryRun?: boolean;
  stubs?: unknown[];
}

const KIND_LABEL: Record<ImportKind, string> = {
  fastmock: 'FastMock stubs',
  wiremock: 'WireMock mappings',
  openapi: 'OpenAPI',
  postman: 'Postman collection',
  har: 'HAR',
  mockserver: 'MockServer expectations',
  unknown: 'detected by the server',
};

const MAX_BYTES = 32 * 1024 * 1024;

export function ImportModal({ opened, onClose }: { opened: boolean; onClose: () => void }) {
  const qc = useQueryClient();
  const [file, setFile] = useState<File | null>(null);
  const [text, setText] = useState('');
  const [kind, setKind] = useState<ImportKind>('unknown');
  const [tag, setTag] = useState('');
  const [replace, setReplace] = useState(true);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<ImportResult | null>(null);

  const reset = () => {
    setFile(null);
    setText('');
    setKind('unknown');
    setTag('');
    setError(null);
    setResult(null);
  };

  const close = () => {
    reset();
    onClose();
  };

  const onDrop = async (files: File[]) => {
    const f = files[0];
    if (!f) return;
    setError(null);
    setResult(null);
    try {
      const t = await readFileText(f, MAX_BYTES);
      setFile(f);
      setText(t);
      setKind(detectImportKind(t).kind);
    } catch (e) {
      setError(errorMessage(e));
    }
  };

  const run = async (dryRun: boolean) => {
    setBusy(true);
    setError(null);
    try {
      let r: ImportResult;
      if (kind === 'fastmock') {
        const parsed = JSON.parse(text) as unknown;
        const stubs = Array.isArray(parsed) ? parsed : (parsed as { stubs: unknown[] }).stubs;
        if (dryRun) {
          r = { imported: 0, removed: 0, dryRun: true, stubs, warnings: [] };
        } else {
          r = await api.post<ImportResult>('/__admin/stubs/import', { stubs, ...(tag.trim() && replace ? { replaceTag: tag.trim() } : {}) });
        }
      } else {
        r = await api.post<ImportResult>('/__admin/import', undefined, {
          rawBody: text,
          contentType: text.trimStart().startsWith('{') || text.trimStart().startsWith('[') ? 'application/json' : 'text/plain',
          query: { tag: tag.trim() || undefined, replace, dryRun: dryRun || undefined },
        });
      }
      setResult(r);
      if (!dryRun) {
        await qc.invalidateQueries({ queryKey: ['stubs'] });
        await qc.invalidateQueries({ queryKey: ['info'] });
        notifySuccess(`Imported ${r.imported} stubs${r.removed ? `, replaced ${r.removed}` : ''}`);
      }
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Modal opened={opened} onClose={close} title="Import stubs" size="lg">
      <Stack gap="md">
        <Dropzone
          onDrop={(f) => void onDrop(f)}
          maxFiles={1}
          maxSize={MAX_BYTES}
          multiple={false}
          onReject={(r) => setError(r[0]?.errors[0]?.message ?? 'File rejected')}
          data-testid="import-dropzone"
        >
          <Group justify="center" gap="md" mih={110} style={{ pointerEvents: 'none' }}>
            <Dropzone.Accept>
              <IconUpload size={40} stroke={1.5} />
            </Dropzone.Accept>
            <Dropzone.Reject>
              <IconX size={40} stroke={1.5} />
            </Dropzone.Reject>
            <Dropzone.Idle>
              <IconFileImport size={40} stroke={1.5} />
            </Dropzone.Idle>
            <div>
              <Text size="md" fw={500}>
                Drop a file here or click to choose
              </Text>
              <Text size="sm" c="dimmed">
                FastMock export, WireMock mappings, OpenAPI 3 (JSON/YAML), Postman v2, HAR, MockServer, AsyncAPI, WSDL…
              </Text>
            </div>
          </Group>
        </Dropzone>
        {file && (
          <Group gap="xs">
            <Text size="sm">
              {file.name} ({Math.ceil(file.size / 1024)} KiB)
            </Text>
            <Badge variant="light">{KIND_LABEL[kind]}</Badge>
          </Group>
        )}
        <TextInput
          label="Tag"
          description={kind === 'fastmock' ? 'Optional: stubs carrying this tag are replaced by the import' : 'Defaults to the detected format name'}
          value={tag}
          onChange={(e) => setTag(e.currentTarget.value)}
          placeholder={kind === 'fastmock' ? 'e.g. team-a' : 'e.g. payments-api'}
        />
        <Checkbox checked={replace} onChange={(e) => setReplace(e.currentTarget.checked)} label="Replace stubs that already carry the tag" />
        {error && (
          <Alert color="red" role="alert">
            {error}
          </Alert>
        )}
        {result && (
          <Alert color={result.dryRun ? 'blue' : 'green'} title={result.dryRun ? 'Preview' : 'Imported'}>
            <Stack gap={4}>
              <Text size="sm">
                {result.dryRun ? `${result.stubs?.length ?? 0} stubs would be created` : `${result.imported} imported, ${result.removed} replaced`}
              </Text>
              {!!result.warnings?.length && (
                <>
                  <Text size="sm" fw={600}>
                    Warnings
                  </Text>
                  <List size="xs" spacing={2}>
                    {result.warnings.slice(0, 30).map((w, i) => (
                      <List.Item key={i}>{w}</List.Item>
                    ))}
                  </List>
                </>
              )}
            </Stack>
          </Alert>
        )}
        <Group justify="flex-end">
          <Button variant="default" onClick={close}>
            Close
          </Button>
          <Button variant="light" disabled={!text} loading={busy} onClick={() => void run(true)}>
            Preview
          </Button>
          <Button disabled={!text} loading={busy} onClick={() => void run(false)} data-testid="import-submit">
            Import
          </Button>
        </Group>
      </Stack>
    </Modal>
  );
}
