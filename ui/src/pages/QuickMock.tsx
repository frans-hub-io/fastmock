import { ActionIcon, Button, Group, NumberInput, SegmentedControl, Stack, Text, TextInput, Title, UnstyledButton } from '@mantine/core';
import {
  IconBolt,
  IconBraces,
  IconDice5,
  IconExternalLink,
  IconHandFinger,
  IconList,
  IconLock,
  IconLogin,
  IconPlus,
  IconTrash,
  IconWand,
  IconWebhook,
} from '@tabler/icons-react';
import { useMemo, useState, type ReactNode } from 'react';
import { Link } from 'react-router';
import { useSpaceChoice } from '../auth/useAuth';
import { CodeEditor } from '../components/CodeEditor';
import { PageHeader, PathText } from '../components/common';
import { SpaceField } from '../components/spaces';
import { docsUrl } from '../lib/docs';
import { buildStubs, formIssues, makeForm, PRESETS, QUICK_METHODS, quickSample, STATUS_CHIPS, type BehaviourKey, type Behaviours, type QuickForm } from '../lib/quickMock';
import { statusTone } from '../lib/ui';
import { BehaviourCards } from './quick/Behaviours';
import { PreviewPanel, type Created } from './quick/Preview';
import classes from './QuickMock.module.css';

const PRESET_ICONS: Record<string, ReactNode> = {
  hello: <IconHandFinger size={18} />,
  login: <IconLogin size={18} />,
  protected: <IconLock size={18} />,
  flaky: <IconDice5 size={18} />,
  paginated: <IconList size={18} />,
  webhook: <IconWebhook size={18} />,
};

function Step({ n, title, description, children, testId }: { n: number; title: string; description: string; children: ReactNode; testId?: string }) {
  const id = `quick-step-${String(n)}`;
  return (
    <section className={classes.step} aria-labelledby={id} data-testid={testId}>
      <div className={classes.stepHead}>
        <span className={classes.stepNumber} aria-hidden>
          {n}
        </span>
        <div>
          <Title order={3} size="h4" id={id}>
            <span className="sr-only">Step {n}: </span>
            {title}
          </Title>
          <Text size="sm" c="dimmed">
            {description}
          </Text>
        </div>
      </div>
      {children}
    </section>
  );
}

function errorFor(issues: { field: string; message: string }[], field: string): string | undefined {
  return issues.find((i) => i.field === field)?.message;
}

export default function QuickMock() {
  const [form, setForm] = useState<QuickForm>(() => makeForm());
  const [preset, setPreset] = useState<string | null>(null);
  const [created, setCreated] = useState<Created | null>(null);
  const [space, setSpace] = useSpaceChoice('stubs:write');

  const set = (fn: (f: QuickForm) => QuickForm) => setForm((f) => fn(structuredClone(f)));
  const setField = (p: Partial<Omit<QuickForm, 'b'>>) => set((f) => ({ ...f, ...p }));
  const setHeader = (i: number, p: Partial<QuickForm['headers'][number]>) => set((f) => ({ ...f, headers: f.headers.map((x, j) => (j === i ? { ...x, ...p } : x)) }));
  const patch = <K extends BehaviourKey>(k: K, p: Partial<Behaviours[K]>) => set((f) => ({ ...f, b: { ...f.b, [k]: { ...f.b[k], ...p } } }));

  const issues = useMemo(() => formIssues(form), [form]);
  const stubs = useMemo(() => buildStubs(form, space), [form, space]);
  const sample = useMemo(() => quickSample(form), [form]);
  const status = form.status === '' ? NaN : form.status;

  const applyPreset = (id: string | null) => {
    const p = PRESETS.find((x) => x.id === id);
    setForm(p ? structuredClone(p.form) : makeForm());
    setPreset(p?.id ?? null);
  };

  const formatBody = () => {
    try {
      set((f) => ({ ...f, body: JSON.stringify(JSON.parse(f.body), null, 2) }));
    } catch {
      /* the issue list already says why */
    }
  };

  return (
    <Stack gap="lg" className={classes.page}>
      <PageHeader
        title="Quick mock"
        description="Build a mock in two steps, watch the stub take shape, create it and call it."
        actions={
          <>
            <Button variant="default" component="a" href={docsUrl('guide/getting-started')} target="_blank" rel="noopener noreferrer" leftSection={<IconExternalLink size={16} />}>
              Guide
            </Button>
            <Button variant="default" component={Link} to="/stubs/new" state={{ stub: stubs[0]?.stub }} leftSection={<IconBraces size={16} />}>
              Open in full editor
            </Button>
          </>
        }
      />

      <div className={classes.layout}>
        <Stack gap="lg" className={classes.main}>
          <section aria-labelledby="quick-templates">
            <Group gap={8} mb={8}>
              <IconWand size={16} aria-hidden className={classes.muted} />
              <Text fw={600} size="sm" id="quick-templates">
                Start from a template
              </Text>
            </Group>
            <div className={classes.presets} role="group" aria-labelledby="quick-templates">
              {PRESETS.map((p) => (
                <UnstyledButton key={p.id} className={classes.preset} data-active={preset === p.id || undefined} aria-pressed={preset === p.id} onClick={() => applyPreset(p.id)} data-testid={`preset-${p.id}`}>
                  <span className={classes.presetIcon} aria-hidden>
                    {PRESET_ICONS[p.id] ?? <IconBolt size={18} />}
                  </span>
                  <span className={classes.presetText}>
                    <Text component="span" fw={600} size="sm" display="block">
                      {p.label}
                    </Text>
                    <Text component="span" size="xs" c="dimmed" display="block">
                      {p.description}
                    </Text>
                  </span>
                </UnstyledButton>
              ))}
              <UnstyledButton className={classes.preset} data-active={preset === null || undefined} aria-pressed={preset === null} onClick={() => applyPreset(null)} data-testid="preset-blank">
                <span className={classes.presetIcon} aria-hidden>
                  <IconPlus size={18} />
                </span>
                <span className={classes.presetText}>
                  <Text component="span" fw={600} size="sm" display="block">
                    Blank
                  </Text>
                  <Text component="span" size="xs" c="dimmed" display="block">
                    GET /hello with a JSON greeting
                  </Text>
                </span>
              </UnstyledButton>
            </div>
          </section>

          <Step n={1} title="Essentials" description="The request to answer and the response to send." testId="quick-step-essentials">
            <Stack gap="md">
              <div className="fm-form-grid">
                <TextInput label="Name" placeholder={`${form.method} ${form.path}`} description="Optional; shown in the stub list" className="fm-span-2" value={form.name} onChange={(e) => setField({ name: e.currentTarget.value })} data-testid="quick-name" />
                {!created && <SpaceField perm="stubs:write" value={space} onChange={setSpace} description="Where the mock goes" />}
              </div>
              <div>
                <Text size="sm" fw={500} mb={4} id="quick-method">
                  Method
                </Text>
                <SegmentedControl
                  value={form.method}
                  onChange={(v) => set((f) => ({ ...f, method: v }))}
                  data={[...QUICK_METHODS]}
                  aria-labelledby="quick-method"
                  className={classes.methods}
                  data-testid="quick-method"
                />
              </div>
              <div>
                <TextInput
                  label="Path"
                  description="Use {name} for path parameters, for example /orders/{id}"
                  value={form.path}
                  onChange={(e) => setField({ path: e.currentTarget.value })}
                  error={errorFor(issues, 'path')}
                  classNames={{ input: 'mono' }}
                  data-testid="quick-path"
                />
                {!errorFor(issues, 'path') && form.path.includes('{') && (
                  <Group gap={6} mt={6} wrap="wrap">
                    <Text size="xs" c="dimmed">
                      Matches
                    </Text>
                    <PathText path={form.path} kind="template" />
                  </Group>
                )}
              </div>
              <div>
                <Group gap="sm" align="flex-end" wrap="wrap">
                  <NumberInput label="Status" value={form.status} onChange={(v) => set((f) => ({ ...f, status: typeof v === 'number' ? v : '' }))} min={200} max={599} w={110} error={errorFor(issues, 'status') ? true : undefined} data-testid="quick-status" />
                  <div className={classes.chips} role="group" aria-label="Common statuses">
                    {STATUS_CHIPS.map((s) => (
                      <button key={s} type="button" className={`fm-status ${classes.chip}`} data-tone={statusTone(s)} aria-pressed={status === s} data-active={status === s || undefined} onClick={() => set((f) => ({ ...f, status: s, bodyType: s === 204 ? 'none' : f.bodyType }))}>
                        {s}
                      </button>
                    ))}
                  </div>
                </Group>
                {errorFor(issues, 'status') && (
                  <Text size="xs" c="var(--mantine-color-error)" mt={4} role="alert">
                    {errorFor(issues, 'status')}
                  </Text>
                )}
              </div>
              <div>
                <Group justify="space-between" mb={6} wrap="wrap" gap="xs">
                  <Text size="sm" fw={500} id="quick-body">
                    Response body
                  </Text>
                  <Group gap="xs">
                    {form.bodyType === 'json' && (
                      <Button size="compact-xs" variant="default" onClick={formatBody} disabled={!!errorFor(issues, 'body')} data-testid="quick-format">
                        Format
                      </Button>
                    )}
                    <SegmentedControl
                      size="xs"
                      value={form.bodyType}
                      onChange={(v) => set((f) => ({ ...f, bodyType: v }))}
                      data={[
                        { label: 'JSON', value: 'json' },
                        { label: 'Text', value: 'text' },
                        { label: 'None', value: 'none' },
                      ]}
                      aria-label="Response body type"
                    />
                  </Group>
                </Group>
                {form.bodyType !== 'none' && (
                  <CodeEditor
                    value={form.body}
                    language={form.bodyType === 'json' ? 'json' : 'text'}
                    minHeight={140}
                    maxHeight={360}
                    label="Response body"
                    testId="quick-body-editor"
                    onChange={(t) => set((f) => ({ ...f, body: t }))}
                  />
                )}
                {errorFor(issues, 'body') && (
                  <Text size="xs" c="var(--mantine-color-error)" mt={4} role="alert">
                    {errorFor(issues, 'body')}
                  </Text>
                )}
              </div>
              <div>
                <Group justify="space-between" mb={6}>
                  <Text size="sm" fw={500}>
                    Response headers
                  </Text>
                  <Button size="compact-sm" variant="subtle" leftSection={<IconPlus size={14} />} onClick={() => set((f) => ({ ...f, headers: [...f.headers, { name: '', value: '' }] }))} data-testid="quick-add-header">
                    Add header
                  </Button>
                </Group>
                {!form.headers.length && (
                  <Text size="xs" c="dimmed">
                    None yet. A JSON body gets content-type: application/json by itself.
                  </Text>
                )}
                <Stack gap={6}>
                  {form.headers.map((h, i) => (
                    <div key={i}>
                      <div className={classes.headerRow}>
                        <TextInput size="xs" placeholder="name" aria-label={`Header ${String(i + 1)} name`} value={h.name} onChange={(e) => setHeader(i, { name: e.currentTarget.value })} classNames={{ input: 'mono' }} error={!!errorFor(issues, `headers.${String(i)}`)} />
                        <TextInput size="xs" placeholder="value" aria-label={`Header ${String(i + 1)} value`} value={h.value} onChange={(e) => setHeader(i, { value: e.currentTarget.value })} classNames={{ input: 'mono' }} />
                        <ActionIcon variant="subtle" color="gray" aria-label={`Remove header ${String(i + 1)}`} onClick={() => set((f) => ({ ...f, headers: f.headers.filter((_, j) => j !== i) }))}>
                          <IconTrash size={16} />
                        </ActionIcon>
                      </div>
                      {errorFor(issues, `headers.${String(i)}`) && (
                        <Text size="xs" c="var(--mantine-color-error)" mt={2} role="alert">
                          {errorFor(issues, `headers.${String(i)}`)}
                        </Text>
                      )}
                    </div>
                  ))}
                </Stack>
              </div>
            </Stack>
          </Step>

          <Step n={2} title="Behaviour" description="Switch on what the mock should do beyond answering. Each card is a real FastMock feature." testId="quick-step-behaviour">
            <BehaviourCards form={form} patch={patch} issues={issues} />
          </Step>
        </Stack>

        <aside className={classes.side} aria-label="Preview and try">
          <PreviewPanel
            stubs={stubs}
            issues={issues}
            space={space}
            created={created}
            onCreated={setCreated}
            onReset={() => {
              setCreated(null);
              applyPreset(null);
            }}
            sample={sample}
            cors={form.b.cors.on}
          />
        </aside>
      </div>
    </Stack>
  );
}
