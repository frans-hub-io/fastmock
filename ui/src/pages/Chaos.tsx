import { ActionIcon, Alert, Badge, Button, Group, NumberInput, Paper, Select, Slider, Stack, Table, Tabs, Text, TextInput } from '@mantine/core';
import { IconDeviceFloppy, IconPlus, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { useComponentSchema } from '../api/features';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import { FAULTS, type ChaosRule } from '../api/types';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { PageHeader } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { pretty } from '../lib/format';


function describe(r: ChaosRule): string {
  const parts: string[] = [];
  if (r.status) parts.push(`status ${r.status}`);
  if (r.fault) parts.push(r.fault);
  if (r.delay?.fixedMs) parts.push(`+${r.delay.fixedMs} ms`);
  if (r.delay?.minMs !== undefined || r.delay?.maxMs !== undefined) parts.push(`+${r.delay.minMs ?? 0}–${r.delay.maxMs ?? 0} ms`);
  return parts.join(', ') || 'no effect';
}

export default function Chaos() {
  const canEdit = useCan('editor');
  const schema = useComponentSchema('ChaosRule');
  const q = useQuery({ queryKey: ['chaos'], queryFn: () => unwrap(client.GET('/__admin/chaos')) });
  const [rules, setRules] = useState<ChaosRule[]>([]);
  const [text, setText] = useState('[]');
  const [jsonError, setJsonError] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [tab, setTab] = useState<string | null>('rules');
  const [draft, setDraft] = useState<ChaosRule>({ id: '', probability: 0.1, status: 503 });

  useEffect(() => {
    if (q.data && !dirty) {
      setRules(q.data.rules);
      setText(pretty(q.data.rules));
    }
  }, [q.data, dirty]);

  const save = useApiMutation<ChaosRule[]>({
    fn: (r) => unwrap(client.PUT('/__admin/chaos', { body: { rules: r } })),
    invalidate: [['chaos']],
    success: 'Chaos rules saved',
    onSuccess: () => setDirty(false),
  });
  const clear = useApiMutation<void>({
    fn: () => unwrap(client.DELETE('/__admin/chaos')),
    invalidate: [['chaos']],
    success: 'Chaos rules removed',
    onSuccess: () => setDirty(false),
  });

  const change = (next: ChaosRule[]) => {
    setRules(next);
    setText(pretty(next));
    setDirty(true);
  };

  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;

  return (
    <Stack gap="md">
      <PageHeader
        title="Chaos"
        description="Inject failures into matching data-plane traffic with a probability: extra latency, error statuses or connection faults. At most 100 rules."
        actions={
          canEdit && (
            <>
              <Button
                variant="light"
                color="red"
                leftSection={<IconTrash size={16} />}
                disabled={!q.data?.rules.length}
                onClick={() => confirmAction({ title: 'Remove all chaos rules?', message: 'Traffic stops being disturbed immediately.', confirm: 'Remove all', onConfirm: () => clear.mutate() })}
              >
                Remove all
              </Button>
              <Button leftSection={<IconDeviceFloppy size={16} />} disabled={!dirty || !!jsonError} loading={save.isPending} onClick={() => save.mutate(rules)}>
                Save rules
              </Button>
            </>
          )
        }
      />
      {dirty && (
        <Alert color="yellow" role="status">
          Unsaved changes.
        </Alert>
      )}
      <Tabs
        value={tab}
        onChange={(t) => {
          if (tab === 'json' && jsonError) return;
          setTab(t);
        }}
      >
        <Tabs.List>
          <Tabs.Tab value="rules">Rules</Tabs.Tab>
          <Tabs.Tab value="json">JSON</Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="rules" pt="md">
          <Stack gap="md">
            {!rules.length ? (
              <EmptyState title="No chaos rules">Traffic is served exactly as the stubs say.</EmptyState>
            ) : (
              <Paper withBorder radius="md">
                <div className="table-scroll">
                  <Table fz="sm">
                    <Table.Thead>
                      <Table.Tr>
                        <Table.Th>Id</Table.Th>
                        <Table.Th>Scope</Table.Th>
                        <Table.Th>Probability</Table.Th>
                        <Table.Th>Effect</Table.Th>
                        <Table.Th>Expires</Table.Th>
                        {canEdit && <Table.Th />}
                      </Table.Tr>
                    </Table.Thead>
                    <Table.Tbody>
                      {rules.map((r, i) => (
                        <Table.Tr key={`${r.id}-${i}`}>
                          <Table.Td className="mono">{r.id}</Table.Td>
                          <Table.Td>
                            <Group gap={4}>
                              {r.method && <Badge size="sm">{r.method}</Badge>}
                              <Text size="sm" className="mono">
                                {r.pathPrefix ?? '/*'}
                              </Text>
                              {r.source && (
                                <Badge size="sm" variant="outline">
                                  {r.source}
                                </Badge>
                              )}
                            </Group>
                          </Table.Td>
                          <Table.Td>{Math.round(r.probability * 100)}%</Table.Td>
                          <Table.Td>{describe(r)}</Table.Td>
                          <Table.Td>{r.expiresAt ?? '—'}</Table.Td>
                          {canEdit && (
                            <Table.Td>
                              <ActionIcon variant="subtle" color="red" aria-label={`Remove rule ${r.id}`} onClick={() => change(rules.filter((_, j) => j !== i))}>
                                <IconTrash size={16} />
                              </ActionIcon>
                            </Table.Td>
                          )}
                        </Table.Tr>
                      ))}
                    </Table.Tbody>
                  </Table>
                </div>
              </Paper>
            )}
            {canEdit && (
              <Paper withBorder p="md" radius="md">
                <Text fw={600} mb="sm">
                  Add a rule
                </Text>
                <Stack gap="sm">
                  <Group gap="sm" wrap="wrap" align="flex-end">
                    <TextInput label="Id" value={draft.id} onChange={(e) => setDraft({ ...draft, id: e.currentTarget.value })} required w={160} />
                    <TextInput label="Path prefix" placeholder="/api" value={draft.pathPrefix ?? ''} onChange={(e) => setDraft({ ...draft, pathPrefix: e.currentTarget.value || undefined })} w={180} />
                    <Select
                      label="Method"
                      data={['GET', 'POST', 'PUT', 'PATCH', 'DELETE']}
                      value={draft.method ?? null}
                      onChange={(v) => setDraft({ ...draft, method: v ?? undefined })}
                      clearable
                      placeholder="any"
                      w={120}
                    />
                    <TextInput label="Source" placeholder="stubs" value={draft.source ?? ''} onChange={(e) => setDraft({ ...draft, source: e.currentTarget.value || undefined })} w={130} />
                  </Group>
                  <div>
                    <Text size="sm" fw={500}>
                      Probability: {Math.round(draft.probability * 100)}%
                    </Text>
                    <Slider value={draft.probability * 100} onChange={(v) => setDraft({ ...draft, probability: v / 100 })} min={0} max={100} step={1} label={(v) => `${v}%`} maw={420} aria-label="Probability" />
                  </div>
                  <Group gap="sm" wrap="wrap" align="flex-end">
                    <NumberInput label="Status" value={draft.status ?? ''} onChange={(v) => setDraft({ ...draft, status: typeof v === 'number' ? v : undefined })} min={100} max={599} w={110} />
                    <Select label="Fault" data={FAULTS} value={draft.fault ?? null} onChange={(v) => setDraft({ ...draft, fault: (v) ?? undefined })} clearable placeholder="none" w={240} />
                    <NumberInput
                      label="Delay ms"
                      value={draft.delay?.fixedMs ?? ''}
                      onChange={(v) => setDraft({ ...draft, delay: typeof v === 'number' ? { fixedMs: v } : undefined })}
                      min={0}
                      max={600000}
                      w={120}
                    />
                  </Group>
                  <Group>
                    <Button
                      variant="light"
                      leftSection={<IconPlus size={16} />}
                      disabled={!draft.id || rules.some((r) => r.id === draft.id) || rules.length >= 100}
                      onClick={() => {
                        const clean = JSON.parse(JSON.stringify(draft)) as ChaosRule;
                        change([...rules, clean]);
                        setDraft({ id: '', probability: 0.1, status: 503 });
                      }}
                    >
                      Add rule
                    </Button>
                  </Group>
                </Stack>
              </Paper>
            )}
          </Stack>
        </Tabs.Panel>
        <Tabs.Panel value="json" pt="md">
          <CodeEditor
            value={text}
            readOnly={!canEdit}
            label="Chaos rules JSON"
            minHeight={300}
            schema={schema ? { root: schema.root, schema: { type: 'array', items: schema.schema } } : null}
            onChange={(t) => {
              setText(t);
              try {
                const v = JSON.parse(t) as unknown;
                if (!Array.isArray(v)) throw new Error('rules must be a JSON array');
                setRules(v as ChaosRule[]);
                setDirty(true);
                setJsonError(null);
              } catch (e) {
                setJsonError(e instanceof Error ? e.message : String(e));
              }
            }}
          />
          {jsonError && (
            <Alert color="red" mt="xs">
              {jsonError}
            </Alert>
          )}
        </Tabs.Panel>
      </Tabs>
    </Stack>
  );
}
