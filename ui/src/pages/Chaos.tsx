import { ActionIcon, Alert, Badge, Button, Group, NumberInput, Select, Slider, Stack, Table, Tabs, Text, TextInput, Tooltip } from '@mantine/core';
import { IconAlertTriangle, IconBraces, IconDeviceFloppy, IconFlame, IconListDetails, IconPlus, IconRestore, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { useComponentSchema } from '../api/features';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import { FAULTS, type ChaosRule } from '../api/types';
import { useCan } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { DataTable, MethodBadge, Meter, PageHeader, SaveBar, SectionCard, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { pretty } from '../lib/format';
import { chaosEffects, chaosRuleIssues } from '../lib/screens';
import type { Tone } from '../lib/ui';

const MAX_RULES = 100;
const EMPTY: ChaosRule = { id: '', probability: 0.1, status: 503 };

function probabilityTone(p: number): Tone {
  if (p >= 0.5) return 'danger';
  if (p >= 0.1) return 'warning';
  return 'info';
}

/** Which paths a rule covers: a prefix, or every path. */
function Scope({ prefix }: { prefix?: string | null }) {
  if (!prefix)
    return (
      <Text size="sm" c="dimmed" component="span" className="fm-nowrap">
        all paths
      </Text>
    );
  return (
    <span className="fm-path">
      <span className="fm-path-kind">prefix</span>
      {prefix}
    </span>
  );
}

function Effects({ rule }: { rule: ChaosRule }) {
  const effects = chaosEffects(rule);
  if (!effects.length)
    return (
      <Text size="sm" c="dimmed">
        no effect
      </Text>
    );
  return (
    <Group gap={4} wrap="wrap">
      {effects.map((e, i) =>
        e.kind === 'status' && e.status ? (
          <StatusBadge key={i} status={e.status} />
        ) : (
          <span key={i} className={e.kind === 'fault' ? 'fm-status' : 'fm-chip'} data-tone={e.kind === 'fault' ? 'danger' : undefined}>
            {e.text}
          </span>
        ),
      )}
    </Group>
  );
}

export default function Chaos() {
  const can = useCan();
  const canEdit = can('chaos:write');
  const schema = useComponentSchema('ChaosRule');
  const q = useQuery({ queryKey: ['chaos'], queryFn: () => unwrap(client.GET('/__admin/chaos')) });
  const [rules, setRules] = useState<ChaosRule[]>([]);
  const [text, setText] = useState('[]');
  const [jsonError, setJsonError] = useState<string | null>(null);
  const [dirty, setDirty] = useState(false);
  const [tab, setTab] = useState<string | null>('rules');
  const [draft, setDraft] = useState<ChaosRule>(EMPTY);
  const [touched, setTouched] = useState(false);

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

  const issues = chaosRuleIssues(draft, rules, MAX_RULES);
  const blocked = Object.keys(issues).length > 0;
  const shown = touched ? issues : { ...issues, id: draft.id ? issues.id : undefined, effect: undefined };
  const active = rules.filter((r) => r.probability > 0).length;

  return (
    <Stack gap="md">
      <PageHeader
        title="Chaos"
        description="Inject failures into matching data-plane traffic with a probability: extra latency, error statuses or connection faults. At most 100 rules."
        meta={
          rules.length > 0 && (
            <>
              <span className="fm-status" data-tone={active ? 'warning' : 'neutral'} data-testid="chaos-active">
                {active ? `${String(active)} rule${active === 1 ? '' : 's'} disturbing traffic` : 'no rule fires'}
              </span>
              <span className="fm-chip">
                {rules.length} / {MAX_RULES}
              </span>
            </>
          )
        }
        actions={
          canEdit && (
            <Button
              variant="light"
              color="red"
              leftSection={<IconTrash size={16} />}
              disabled={!q.data?.rules.length}
              onClick={() => confirmAction({ title: 'Remove all chaos rules?', message: 'Traffic stops being disturbed immediately.', confirm: 'Remove all', onConfirm: () => clear.mutate() })}
            >
              Remove all
            </Button>
          )
        }
      />
      <Tabs
        value={tab}
        variant="pills"
        radius="md"
        classNames={{ list: 'fm-tabs-list' }}
        onChange={(t) => {
          if (tab === 'json' && jsonError) return;
          setTab(t);
        }}
      >
        <Tabs.List>
          <Tabs.Tab value="rules" leftSection={<IconListDetails size={16} />}>
            Rules <span className="fm-tab-count">{rules.length}</span>
          </Tabs.Tab>
          <Tabs.Tab value="json" leftSection={<IconBraces size={16} />}>
            JSON
          </Tabs.Tab>
        </Tabs.List>
        <Tabs.Panel value="rules" pt="md">
          <Stack gap="md">
            {!rules.length ? (
              <EmptyState title="No chaos rules" icon={<IconFlame size={30} />} docs={{ label: 'Proxying and chaos', href: docsUrl('guide/proxy-and-chaos') }}>
                Traffic is served exactly as the stubs say. {canEdit ? 'Add a rule below to add latency, errors or dropped connections to a share of requests.' : ''}
              </EmptyState>
            ) : (
              <DataTable testId="chaos-table" miw={900}>
                <Table.Thead>
                  <Table.Tr>
                    <Table.Th>Rule</Table.Th>
                    <Table.Th>Applies to</Table.Th>
                    <Table.Th w={170}>Probability</Table.Th>
                    <Table.Th>Effect</Table.Th>
                    <Table.Th>Expires</Table.Th>
                    {canEdit && (
                      <Table.Th w={56}>
                        <span className="sr-only">Actions</span>
                      </Table.Th>
                    )}
                  </Table.Tr>
                </Table.Thead>
                <Table.Tbody>
                  {rules.map((r, i) => (
                    <Table.Tr key={`${r.id}-${String(i)}`} data-testid="chaos-row">
                      <Table.Td>
                        <Text size="sm" fw={700} className="mono break-all">
                          {r.id}
                        </Text>
                      </Table.Td>
                      <Table.Td data-label="Applies to">
                        <Group gap={6} wrap="nowrap">
                          <MethodBadge method={r.method} />
                          <Scope prefix={r.pathPrefix} />
                          {r.source && <span className="fm-chip">{r.source}</span>}
                        </Group>
                      </Table.Td>
                      <Table.Td data-label="Probability">
                        <Meter value={r.probability} tone={probabilityTone(r.probability)} label={`Probability of ${r.id}`} />
                      </Table.Td>
                      <Table.Td data-label="Effect">
                        <Effects rule={r} />
                      </Table.Td>
                      <Table.Td data-label="Expires">{r.expiresAt ? <Time at={r.expiresAt} /> : <Text size="sm" c="dimmed">never</Text>}</Table.Td>
                      {canEdit && (
                        <Table.Td>
                          <Group justify="flex-end">
                            <Tooltip label="Remove rule">
                              <ActionIcon variant="subtle" color="red" aria-label={`Remove rule ${r.id}`} onClick={() => change(rules.filter((_, j) => j !== i))}>
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
            {canEdit && (
              <SectionCard title="Add a rule" description="Rules apply in order; the first matching rule that fires disturbs the request" icon={<IconPlus size={16} />}>
                <form
                  onSubmit={(e) => {
                    e.preventDefault();
                    setTouched(true);
                    if (blocked) return;
                    const clean = JSON.parse(JSON.stringify({ ...draft, id: draft.id.trim() })) as ChaosRule;
                    change([...rules, clean]);
                    setDraft(EMPTY);
                    setTouched(false);
                  }}
                >
                  <Stack gap="md">
                    <div className="fm-form-grid">
                      <TextInput label="Id" placeholder="payments-outage" value={draft.id} onChange={(e) => setDraft({ ...draft, id: e.currentTarget.value })} required error={shown.id} classNames={{ input: 'mono' }} />
                      <TextInput label="Path prefix" placeholder="/api" value={draft.pathPrefix ?? ''} onChange={(e) => setDraft({ ...draft, pathPrefix: e.currentTarget.value || undefined })} classNames={{ input: 'mono' }} />
                      <Select
                        label="Method"
                        data={['GET', 'POST', 'PUT', 'PATCH', 'DELETE']}
                        value={draft.method ?? null}
                        onChange={(v) => setDraft({ ...draft, method: v ?? undefined })}
                        clearable
                        placeholder="any"
                      />
                      <TextInput label="Source" placeholder="stubs" value={draft.source ?? ''} onChange={(e) => setDraft({ ...draft, source: e.currentTarget.value || undefined })} />
                    </div>
                    <div>
                      <Group justify="space-between" mb={4}>
                        <Text size="sm" fw={500} id="chaos-probability">
                          Probability
                        </Text>
                        <Badge variant="light" tt="none" className="tabular">
                          {Math.round(draft.probability * 100)}% of matching requests
                        </Badge>
                      </Group>
                      <Slider
                        value={draft.probability * 100}
                        onChange={(v) => setDraft({ ...draft, probability: v / 100 })}
                        min={0}
                        max={100}
                        step={1}
                        label={(v) => `${String(v)}%`}
                        marks={[{ value: 0 }, { value: 25 }, { value: 50 }, { value: 75 }, { value: 100 }]}
                        aria-labelledby="chaos-probability"
                        thumbLabel="Probability"
                      />
                    </div>
                    <div className="fm-form-grid">
                      <NumberInput label="Status" value={draft.status ?? ''} onChange={(v) => setDraft({ ...draft, status: typeof v === 'number' ? v : undefined })} min={100} max={599} error={shown.status} />
                      <Select label="Fault" data={FAULTS} value={draft.fault ?? null} onChange={(v) => setDraft({ ...draft, fault: v ?? undefined })} clearable placeholder="none" className="fm-span-2" />
                      <NumberInput
                        label="Delay"
                        suffix=" ms"
                        value={draft.delay?.fixedMs ?? ''}
                        onChange={(v) => setDraft({ ...draft, delay: typeof v === 'number' ? { fixedMs: v } : undefined })}
                        min={0}
                        max={600000}
                      />
                    </div>
                    {(shown.effect || issues.limit) && (
                      <Alert color="yellow" icon={<IconAlertTriangle size={18} />} p="xs">
                        {issues.limit ?? shown.effect}
                      </Alert>
                    )}
                    <Group justify="space-between" wrap="wrap" gap="sm">
                      <Group gap={6} wrap="wrap">
                        <Text size="sm" c="dimmed">
                          Preview
                        </Text>
                        <MethodBadge method={draft.method} />
                        <Scope prefix={draft.pathPrefix} />
                        <Text size="sm" c="dimmed">
                          →
                        </Text>
                        <Effects rule={draft} />
                      </Group>
                      <Group gap="xs">
                        <Button variant="subtle" color="gray" leftSection={<IconRestore size={16} />} onClick={() => {
                          setDraft(EMPTY);
                          setTouched(false);
                        }}>
                          Reset
                        </Button>
                        <Button type="submit" variant="light" leftSection={<IconPlus size={16} />} disabled={!!issues.limit}>
                          Add rule
                        </Button>
                      </Group>
                    </Group>
                  </Stack>
                </form>
              </SectionCard>
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
            <Alert color="red" mt="xs" icon={<IconAlertTriangle size={18} />}>
              {jsonError}
            </Alert>
          )}
        </Tabs.Panel>
      </Tabs>
      {canEdit && (
        <SaveBar
          dirty={dirty}
          testId="chaos-save-bar"
          status={
            jsonError ? (
              <span className="fm-status" data-tone="danger">
                invalid JSON
              </span>
            ) : (
              <span className="fm-chip">
                {rules.length} rule{rules.length === 1 ? '' : 's'}
              </span>
            )
          }
        >
          {dirty && (
            <Button
              variant="subtle"
              color="gray"
              onClick={() => {
                setDirty(false);
                setJsonError(null);
                if (q.data) {
                  setRules(q.data.rules);
                  setText(pretty(q.data.rules));
                }
              }}
            >
              Discard
            </Button>
          )}
          <Button leftSection={<IconDeviceFloppy size={16} />} disabled={!dirty || !!jsonError} loading={save.isPending} onClick={() => save.mutate(rules)} data-testid="save-chaos">
            Save rules
          </Button>
        </SaveBar>
      )}
    </Stack>
  );
}
