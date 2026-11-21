import {
  Alert,
  Badge,
  Button,
  Fieldset,
  Group,
  Kbd,
  NumberInput,
  Paper,
  SegmentedControl,
  Select,
  Stack,
  Switch,
  Tabs,
  TagsInput,
  Text,
  TextInput,
  Tooltip,
} from '@mantine/core';
import { useDebouncedValue, useHotkeys } from '@mantine/hooks';
import {
  IconAdjustmentsHorizontal,
  IconArrowBackUp,
  IconArrowDownLeft,
  IconArrowUpRight,
  IconCheck,
  IconCode,
  IconDeviceFloppy,
  IconForms,
  IconId,
  IconLoader2,
  IconSend,
  IconTrash,
  IconX,
} from '@tabler/icons-react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useMemo, useState } from 'react';
import { useLocation, useNavigate, useParams } from 'react-router';
import { useComponentSchema, useStubSchema } from '../api/features';
import { client, unwrap } from '../api/client';
import { errorMessage } from '../api/http';
import { confirmAction, notifyError, useApiMutation } from '../api/mutations';
import { FAULTS, type BodyMatcher, type Delay, type ResponseDef, type Stub } from '../api/types';
import { useCan, useSpace, useSpaceChoice } from '../auth/useAuth';
import { SpaceBadge, SpaceField } from '../components/spaces';
import { CodeEditor } from '../components/CodeEditor';
import { MethodBadge, PageHeader, PathText, SectionCard, ToneBadge } from '../components/common';
import { KeyValueEditor, MatcherList } from '../components/MatcherList';
import { ErrorState, LoadingState } from '../components/States';
import { TestRequestPanel } from '../components/TestRequestPanel';
import { pretty } from '../lib/format';
import { pathKindOf } from '../lib/path';
import { sampleRequest, setUrl, stripServerFields, stubPath, stubTitle, URL_KIND_LABELS, urlKind, urlValue, type UrlKind } from '../lib/stubs';
import classes from './StubEditor.module.css';

const METHODS = ['ANY', 'GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'];

const NEW_STUB: Stub = {
  name: '',
  priority: 5,
  request: { method: 'GET', urlPath: '/hello' },
  response: { status: 200, headers: { 'content-type': 'application/json' }, jsonBody: { hello: 'world' } },
};

type BodyKind = 'none' | 'json' | 'text' | 'base64';
type DelayKind = 'none' | 'fixed' | 'random';

function bodyKind(r: ResponseDef | undefined): BodyKind {
  if (!r) return 'none';
  if (r.jsonBody !== undefined) return 'json';
  if (typeof r.base64Body === 'string') return 'base64';
  if (typeof r.body === 'string') return 'text';
  return 'none';
}

function delayKind(d: Delay | null | undefined): DelayKind {
  if (!d) return 'none';
  if (d.fixedMs != null) return 'fixed';
  if (d.minMs != null || d.maxMs != null) return 'random';
  return 'none';
}

function clean<T extends Record<string, unknown>>(o: T): T {
  const out = { ...o };
  for (const k of Object.keys(out)) if (out[k] === undefined) delete out[k];
  return out;
}

/** The texts of the body editors for a stub. */
function editorTexts(d: Stub): { resp: string; req: string } {
  const r = d.response;
  const k = bodyKind(r);
  return {
    resp: k === 'json' ? pretty(r?.jsonBody) : k === 'base64' ? (r?.base64Body ?? '') : k === 'text' ? (r?.body ?? '') : '',
    req: d.request.body?.length ? pretty(d.request.body) : '',
  };
}

interface ValidateResult {
  valid: boolean;
  error?: string;
  errors?: string[];
  details?: string[];
}

export default function StubEditor() {
  const { id } = useParams();
  const isNew = !id;
  const loc = useLocation();
  const navigate = useNavigate();
  const qc = useQueryClient();
  const can = useCan();
  const spaces = useSpace();
  const stubSchema = useStubSchema();
  const bodyMatcherSchema = useComponentSchema('BodyMatcher');

  const existing = useQuery({
    queryKey: ['stub', id],
    queryFn: () => unwrap(client.GET('/__admin/stubs/{id}', { params: { path: { id: id ?? '' } } })),
    enabled: !isNew,
  });
  const where = isNew ? undefined : (existing.data?.space ?? null);
  const canEdit = can('stubs:write', where);
  const readOnly = !canEdit;

  const prefill = (loc.state as { stub?: Stub } | null)?.stub;
  const [draft, setDraft] = useState<Stub | null>(isNew ? structuredClone(prefill ?? NEW_STUB) : null);
  const [tab, setTab] = useState<string>('form');
  const [jsonText, setJsonText] = useState('');
  const [jsonError, setJsonError] = useState<string | null>(null);
  const [lintCount, setLintCount] = useState(0);
  const [respBodyText, setRespBodyText] = useState('');
  const [reqBodyText, setReqBodyText] = useState('');
  const [respBodyError, setRespBodyError] = useState<string | null>(null);
  const [reqBodyError, setReqBodyError] = useState<string | null>(null);
  const [baseline, setBaseline] = useState<string | null>(isNew ? null : '');

  useEffect(() => {
    if (existing.data && !draft) {
      const d = stripServerFields(existing.data);
      setDraft(d);
      setBaseline(JSON.stringify(d));
    }
  }, [existing.data, draft]);

  useEffect(() => {
    if (!draft) return;
    const t = editorTexts(draft);
    setRespBodyText(t.resp);
    setReqBodyText(t.req);
    // re-sync editor texts only when the draft identity is replaced from outside
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [draft === null, existing.data]);

  const [debouncedDraft] = useDebouncedValue(draft, 600);
  const validation = useQuery({
    queryKey: ['stub-validate', debouncedDraft],
    queryFn: () => unwrap(client.POST('/__admin/stubs/validate', { body: debouncedDraft as Stub })) as Promise<ValidateResult>,
    enabled: !!debouncedDraft && !jsonError,
    retry: false,
    staleTime: Infinity,
  });

  const save = useApiMutation<Stub, Stub>({
    fn: (s) => (isNew || !id ? unwrap(client.POST('/__admin/stubs', { body: s })) : unwrap(client.PUT('/__admin/stubs/{id}', { params: { path: { id } }, body: s }))),
    invalidate: [['stubs'], ['info']],
    success: (d) => `Saved ${stubTitle(d)}`,
    onSuccess: (d) => {
      qc.setQueryData(['stub', d.id], d);
      setBaseline(JSON.stringify(stripServerFields(d)));
      if (isNew && d.id) void navigate(`/stubs/${d.id}`, { replace: true });
    },
  });
  const remove = useApiMutation<void>({
    fn: () => unwrap(client.DELETE('/__admin/stubs/{id}', { params: { path: { id: id ?? '' } } })),
    invalidate: [['stubs'], ['info']],
    success: 'Stub deleted',
    onSuccess: () => void navigate('/stubs'),
  });

  const sample = useMemo(() => (draft ? sampleRequest(draft) : null), [draft]);
  const [newSpace, setNewSpace] = useSpaceChoice('stubs:write', prefill?.space);
  useEffect(() => {
    if (isNew) setDraft((d) => (d && (d.space ?? null) !== newSpace ? clean({ ...d, space: newSpace ?? undefined }) : d));
  }, [isNew, newSpace]);

  useHotkeys([['mod+S', () => document.querySelector<HTMLButtonElement>('[data-testid="save-stub"]:not(:disabled)')?.click()]], []);

  if (!isNew && existing.isLoading) return <LoadingState />;
  if (!isNew && existing.isError) return <ErrorState error={existing.error} onRetry={() => void existing.refetch()} />;
  if (!draft) return <LoadingState />;

  const update = (fn: (d: Stub) => Stub) => setDraft((d) => (d ? fn(structuredClone(d)) : d));
  const updateResponse = (fn: (r: ResponseDef) => ResponseDef) => update((d) => ({ ...d, response: clean(fn({ ...(d.response ?? {}) })) }));

  const switchTab = (next: string | null) => {
    if (!next) return;
    if (tab === 'json' && next !== 'json' && jsonError) {
      notifyError(new Error('Fix the JSON before leaving the JSON tab'), 'Invalid JSON');
      return;
    }
    if (next === 'json') {
      setJsonText(pretty(draft));
      setJsonError(null);
    }
    setTab(next);
  };

  const onJsonChange = (text: string) => {
    setJsonText(text);
    try {
      const v = JSON.parse(text) as unknown;
      if (!v || typeof v !== 'object' || Array.isArray(v)) throw new Error('A stub must be a JSON object');
      if (!('request' in v)) throw new Error('A stub needs a "request" object');
      setDraft(v as Stub);
      setJsonError(null);
    } catch (e) {
      setJsonError(errorMessage(e));
    }
  };

  const doSave = () => {
    if (jsonError) {
      notifyError(new Error(jsonError), 'Invalid JSON');
      return;
    }
    const s = structuredClone(draft);
    if (!s.name) delete s.name;
    save.mutate(s);
  };

  const kind = urlKind(draft.request);
  const bk = bodyKind(draft.response);
  const dk = delayKind(draft.response?.delay);
  const serverErrors = validation.data && !validation.data.valid ? [validation.data.error, ...(validation.data.errors ?? []), ...(validation.data.details ?? [])].filter(Boolean) : [];
  const url = urlValue(draft.request);
  const urlError =
    kind === 'any' ? null : !url.trim() ? 'Enter a path, for example /orders' : (kind === 'urlPath' || kind === 'urlPathTemplate' || kind === 'url') && !url.startsWith('/') ? 'A path starts with /' : null;
  const status = draft.response?.status ?? 200;
  const statusError = status < 100 || status > 599 ? 'Use an HTTP status from 100 to 599' : null;
  const delay = draft.response?.delay;
  const delayError = dk === 'random' && (delay?.minMs ?? 0) > (delay?.maxMs ?? 0) ? 'Max must not be below min' : null;
  const fieldErrors = [urlError, statusError, delayError, respBodyError, reqBodyError].filter(Boolean).length;
  const dirty = baseline !== null && baseline !== '' ? JSON.stringify(draft) !== baseline : isNew;
  const title = isNew ? 'New stub' : stubTitle(draft);

  const validationBadge = jsonError ? (
    <ToneBadge tone="danger">invalid JSON</ToneBadge>
  ) : validation.isFetching ? (
    <Badge color="gray" variant="light" leftSection={<IconLoader2 size={12} className={classes.spin} />} tt="none">
      validating…
    </Badge>
  ) : validation.data?.valid ? (
    <Badge color="green" variant="light" leftSection={<IconCheck size={12} />} data-testid="validation-ok" tt="none">
      server accepts this stub
    </Badge>
  ) : validation.data || validation.isError ? (
    <Badge color="red" variant="light" leftSection={<IconX size={12} />} tt="none">
      server rejects this stub
    </Badge>
  ) : null;

  return (
    <Stack gap="md" className={classes.page}>
      <PageHeader
        title={title}
        breadcrumbs={[{ label: 'Stubs', to: '/stubs' }, { label: title }]}
        description={
          isNew ? (
            'Describe the request to match and the response FastMock returns.'
          ) : (
            <Group gap={6} wrap="nowrap" component="span">
              <IconId size={14} aria-hidden />
              <Text span className="mono" size="xs">
                {id}
              </Text>
            </Group>
          )
        }
        meta={
          <>
            <MethodBadge method={draft.request.method} />
            <PathText path={stubPath(draft)} kind={pathKindOf(kind)} />
            {draft.enabled === false && <ToneBadge tone="neutral">disabled</ToneBadge>}
            {spaces.enabled && !isNew && <SpaceBadge space={draft.space} />}
          </>
        }
        actions={
          canEdit ? (
            !isNew && (
              <Button
                variant="subtle"
                color="red"
                leftSection={<IconTrash size={16} />}
                onClick={() =>
                  confirmAction({
                    title: 'Delete stub?',
                    message: `"${stubTitle(draft)}" will be removed. This cannot be undone.`,
                    confirm: 'Delete',
                    onConfirm: () => remove.mutate(),
                  })
                }
              >
                Delete
              </Button>
            )
          ) : (
            <Tooltip label={can.missing('stubs:write', where)} withArrow>
              <Badge color="gray" variant="light" tabIndex={0} size="lg" tt="none">
                read-only
              </Badge>
            </Tooltip>
          )
        }
      />

      {(serverErrors.length > 0 || validation.isError) && (
        <Alert color="red" title="The server rejects this stub" role="alert" icon={<IconX size={18} />}>
          {serverErrors.length ? serverErrors.map((e, i) => <Text key={i} size="sm">{e}</Text>) : errorMessage(validation.error)}
        </Alert>
      )}

      <Tabs value={tab} onChange={switchTab} keepMounted={false} variant="pills" radius="md" classNames={{ list: classes.tabs }}>
        <Tabs.List>
          <Tabs.Tab value="form" leftSection={<IconForms size={16} />}>
            Form
          </Tabs.Tab>
          <Tabs.Tab value="json" leftSection={<IconCode size={16} />} data-testid="tab-json">
            JSON
          </Tabs.Tab>
          <Tabs.Tab value="test" leftSection={<IconSend size={16} />} data-testid="tab-test">
            Test this stub
          </Tabs.Tab>
          {tab === 'json' && lintCount > 0 && (
            <Badge color="orange" variant="light" ml="auto" tt="none" className={classes.lint}>
              {lintCount} schema issue{lintCount === 1 ? '' : 's'}
            </Badge>
          )}
        </Tabs.List>

        <Tabs.Panel value="form" pt="md">
          <Stack gap="lg">
            <SectionCard title="Basics" description="How the stub is named, grouped and switched on" icon={<IconId size={16} />}>
              <Fieldset variant="unstyled" disabled={readOnly} p={0} m={0}>
                <div className={classes.basics}>
                  {isNew ? (
                    <SpaceField perm="stubs:write" value={newSpace} onChange={setNewSpace} description="The stub's space; it cannot be moved later" />
                  ) : null}
                  <TextInput
                    label="Name"
                    placeholder="e.g. create order"
                    value={draft.name ?? ''}
                    onChange={(e) => {
                      const v = e.currentTarget.value;
                      update((d) => ({ ...d, name: v }));
                    }}
                    data-testid="stub-name"
                    className={classes.name}
                  />
                  <TagsInput
                    label="Tags"
                    placeholder="Type and press Enter"
                    value={draft.tags ?? []}
                    onChange={(v) => update((d) => clean({ ...d, tags: v.length ? v : undefined }))}
                    className={classes.tags}
                  />
                  <Switch
                    label="Enabled"
                    description="Disabled stubs never match"
                    checked={draft.enabled !== false}
                    onChange={(e) => {
                      const v = e.currentTarget.checked;
                      update((d) => clean({ ...d, enabled: v ? undefined : false }));
                    }}
                    className={classes.enabled}
                  />
                </div>
              </Fieldset>
            </SectionCard>

            <div className={classes.split}>
              <SectionCard title="Request" description="What an incoming request must look like" icon={<IconArrowDownLeft size={16} />}>
                <Fieldset variant="unstyled" disabled={readOnly} p={0} m={0}>
                  <Stack gap="md">
                    <Group align="flex-start" gap="sm" wrap="wrap">
                      <Select
                        label="Method"
                        data={METHODS}
                        value={(draft.request.method ?? 'ANY').toUpperCase()}
                        onChange={(v) => update((d) => ({ ...d, request: clean({ ...d.request, method: v && v !== 'ANY' ? v : undefined }) }))}
                        allowDeselect={false}
                        w={120}
                        data-testid="stub-method"
                      />
                      <Select
                        label="URL match"
                        data={Object.entries(URL_KIND_LABELS).map(([value, label]) => ({ value, label }))}
                        value={kind}
                        onChange={(v) => v && update((d) => ({ ...d, request: setUrl(d.request, v as UrlKind, urlValue(d.request)) }))}
                        allowDeselect={false}
                        w={190}
                      />
                    </Group>
                    {kind !== 'any' && (
                      <TextInput
                        label="URL"
                        value={url}
                        onChange={(e) => {
                          const v = e.currentTarget.value;
                          update((d) => ({ ...d, request: setUrl(d.request, kind, v) }));
                        }}
                        classNames={{ input: 'mono' }}
                        placeholder={kind === 'urlPathTemplate' ? '/orders/{id}' : '/path'}
                        description={kind === 'urlPathTemplate' ? 'Name parameters in braces, e.g. /orders/{id}' : kind === 'urlPathPattern' || kind === 'urlPattern' ? 'A regular expression matched against the whole path' : undefined}
                        error={urlError}
                        data-testid="stub-url"
                      />
                    )}
                    <MatcherList
                      label="Query parameters"
                      keyLabel="parameter"
                      value={draft.request.query}
                      onChange={(v) => update((d) => ({ ...d, request: clean({ ...d.request, query: v }) }))}
                      readOnly={readOnly}
                    />
                    <MatcherList
                      label="Headers"
                      keyLabel="header"
                      value={draft.request.headers}
                      onChange={(v) => update((d) => ({ ...d, request: clean({ ...d.request, headers: v }) }))}
                      readOnly={readOnly}
                    />
                    {kind === 'urlPathTemplate' && (
                      <MatcherList
                        label="Path parameters"
                        keyLabel="parameter"
                        value={draft.request.pathParams}
                        onChange={(v) => update((d) => ({ ...d, request: clean({ ...d.request, pathParams: v }) }))}
                        readOnly={readOnly}
                      />
                    )}
                    <div>
                      <Text size="sm" fw={500} mb={2}>
                        Body matchers
                      </Text>
                      <Text size="xs" c="dimmed" mb={6}>
                        A JSON array, e.g. {'[{"jsonPath": "$.to", "value": {"matches": "\\d+"}}]'} or {'[{"equalToJson": {...}, "ignoreExtraElements": true}]'}
                      </Text>
                      <CodeEditor
                        value={reqBodyText}
                        readOnly={readOnly}
                        minHeight={90}
                        maxHeight={320}
                        label="Request body matchers"
                        schema={bodyMatcherSchema ? { root: bodyMatcherSchema.root, schema: { type: 'array', items: bodyMatcherSchema.schema } } : null}
                        onChange={(t) => {
                          setReqBodyText(t);
                          if (!t.trim()) {
                            setReqBodyError(null);
                            update((d) => ({ ...d, request: clean({ ...d.request, body: undefined }) }));
                            return;
                          }
                          try {
                            const v = JSON.parse(t) as unknown;
                            if (Array.isArray(v)) {
                              setReqBodyError(null);
                              update((d) => ({ ...d, request: { ...d.request, body: v as BodyMatcher[] } }));
                            } else setReqBodyError('Body matchers must be a JSON array');
                          } catch (e) {
                            setReqBodyError(`Invalid JSON: ${errorMessage(e)}. The last valid value is kept.`);
                          }
                        }}
                      />
                      {reqBodyError && (
                        <Text size="xs" c="var(--mantine-color-error)" mt={4} role="alert">
                          {reqBodyError}
                        </Text>
                      )}
                    </div>
                  </Stack>
                </Fieldset>
              </SectionCard>

              <SectionCard title="Response" description="What FastMock answers with" icon={<IconArrowUpRight size={16} />}>
                <Fieldset variant="unstyled" disabled={readOnly} p={0} m={0}>
                  <Stack gap="md">
                    <Group align="flex-start" gap="sm" wrap="wrap">
                      <NumberInput
                        label="Status"
                        value={status}
                        onChange={(v) => updateResponse((r) => ({ ...r, status: typeof v === 'number' ? v : 200 }))}
                        min={100}
                        max={599}
                        w={120}
                        error={statusError}
                        data-testid="stub-status"
                      />
                      <Switch
                        label="Handlebars templating"
                        description="Render {{…}} expressions"
                        checked={draft.response?.templated !== false}
                        onChange={(e) => {
                          const v = e.currentTarget.checked;
                          updateResponse((r) => ({ ...r, templated: v ? undefined : false }));
                        }}
                        mt={28}
                      />
                    </Group>
                    <KeyValueEditor
                      multi
                      label="Response headers"
                      value={draft.response?.headers}
                      onChange={(v) => updateResponse((r) => ({ ...r, headers: v }))}
                      readOnly={readOnly}
                    />
                    <div>
                      <Group justify="space-between" mb={6} wrap="wrap" gap="xs">
                        <Text size="sm" fw={500}>
                          Body
                        </Text>
                        <SegmentedControl
                          size="xs"
                          value={bk}
                          disabled={readOnly}
                          onChange={(v) => {
                            const next = v;
                            setRespBodyError(null);
                            updateResponse((r) => {
                              const out: ResponseDef = { ...r, body: undefined, jsonBody: undefined, base64Body: undefined };
                              if (next === 'json') {
                                let parsed: unknown;
                                try {
                                  parsed = respBodyText.trim() ? JSON.parse(respBodyText) : {};
                                } catch {
                                  parsed = respBodyText;
                                }
                                out.jsonBody = parsed;
                              } else if (next === 'text') out.body = respBodyText;
                              else if (next === 'base64') out.base64Body = '';
                              return out;
                            });
                            if (next === 'none') setRespBodyText('');
                          }}
                          data={[
                            { label: 'None', value: 'none' },
                            { label: 'JSON', value: 'json' },
                            { label: 'Text', value: 'text' },
                            { label: 'Base64', value: 'base64' },
                          ]}
                          aria-label="Response body type"
                        />
                      </Group>
                      {bk !== 'none' && (
                        <CodeEditor
                          value={respBodyText}
                          language={bk === 'json' ? 'json' : respBodyText.trimStart().startsWith('<') ? 'xml' : 'text'}
                          readOnly={readOnly}
                          minHeight={160}
                          label="Response body"
                          testId="response-body-editor"
                          onChange={(t) => {
                            setRespBodyText(t);
                            if (bk === 'json') {
                              try {
                                const v = JSON.parse(t) as unknown;
                                setRespBodyError(null);
                                updateResponse((r) => ({ ...r, jsonBody: v }));
                              } catch (e) {
                                setRespBodyError(`Invalid JSON: ${errorMessage(e)}. The last valid value is kept.`);
                              }
                            } else if (bk === 'text') updateResponse((r) => ({ ...r, body: t }));
                            else updateResponse((r) => ({ ...r, base64Body: t.trim() }));
                          }}
                        />
                      )}
                      {respBodyError && (
                        <Text size="xs" c="var(--mantine-color-error)" mt={4} role="alert">
                          {respBodyError}
                        </Text>
                      )}
                      <Text size="xs" c="dimmed" mt={6}>
                        Templates use Handlebars, e.g. {'{{request.pathParams.id}}'}, {'{{uuid}}'}, {'{{now}}'}, {'{{jsonPath request.body "$.id"}}'}.
                      </Text>
                    </div>
                  </Stack>
                </Fieldset>
              </SectionCard>
            </div>

            <SectionCard title="Advanced" description="Matching order, timing, faults and scenario state" icon={<IconAdjustmentsHorizontal size={16} />}>
              <Fieldset variant="unstyled" disabled={readOnly} p={0} m={0}>
                <Stack gap="lg">
                  <div className={classes.advanced}>
                    <NumberInput
                      label="Priority"
                      description="Lower wins when several stubs match"
                      value={draft.priority ?? 5}
                      onChange={(v) => update((d) => ({ ...d, priority: typeof v === 'number' ? v : 5 }))}
                      min={-1000000}
                      max={1000000}
                    />
                    <Select
                      label="Fault"
                      description="Break the connection instead of answering"
                      data={FAULTS}
                      value={draft.response?.fault ?? null}
                      onChange={(v) => updateResponse((r) => ({ ...r, fault: v ?? undefined }))}
                      clearable
                      placeholder="none"
                    />
                  </div>
                  <div>
                    <Text size="sm" fw={500} mb={6}>
                      Delay
                    </Text>
                    <Group align="flex-start" gap="sm" wrap="wrap">
                      <SegmentedControl
                        value={dk}
                        disabled={readOnly}
                        onChange={(v) =>
                          updateResponse((r) => ({
                            ...r,
                            delay: v === 'none' ? undefined : v === 'fixed' ? { fixedMs: r.delay?.fixedMs ?? 100 } : { minMs: r.delay?.minMs ?? 50, maxMs: r.delay?.maxMs ?? 200 },
                          }))
                        }
                        data={[
                          { label: 'No delay', value: 'none' },
                          { label: 'Fixed', value: 'fixed' },
                          { label: 'Random', value: 'random' },
                        ]}
                        aria-label="Delay type"
                      />
                      {dk === 'fixed' && (
                        <NumberInput
                          aria-label="Delay ms"
                          placeholder="Delay ms"
                          rightSection={<Text size="xs" c="dimmed">ms</Text>}
                          value={draft.response?.delay?.fixedMs ?? 0}
                          min={0}
                          max={600000}
                          onChange={(v) => updateResponse((r) => ({ ...r, delay: { fixedMs: typeof v === 'number' ? v : 0 } }))}
                          w={140}
                        />
                      )}
                      {dk === 'random' && (
                        <>
                          <NumberInput
                            aria-label="Min ms"
                            leftSection={<Text size="xs" c="dimmed">min</Text>}
                            rightSection={<Text size="xs" c="dimmed">ms</Text>}
                            value={draft.response?.delay?.minMs ?? 0}
                            min={0}
                            max={600000}
                            onChange={(v) => updateResponse((r) => ({ ...r, delay: { ...r.delay, minMs: typeof v === 'number' ? v : 0 } }))}
                            w={150}
                          />
                          <NumberInput
                            aria-label="Max ms"
                            leftSection={<Text size="xs" c="dimmed">max</Text>}
                            rightSection={<Text size="xs" c="dimmed">ms</Text>}
                            value={draft.response?.delay?.maxMs ?? 0}
                            min={0}
                            max={600000}
                            onChange={(v) => updateResponse((r) => ({ ...r, delay: { ...r.delay, maxMs: typeof v === 'number' ? v : 0 } }))}
                            error={delayError}
                            w={150}
                          />
                        </>
                      )}
                    </Group>
                  </div>
                  <div>
                    <Text size="sm" fw={500} mb={6}>
                      Scenario
                    </Text>
                    <div className={classes.scenario}>
                      <TextInput
                        label="Scenario name"
                        description="May be a template, e.g. order-{{request.pathParams.id}}"
                        value={draft.scenario?.name ?? ''}
                        onChange={(e) => {
                          const v = e.currentTarget.value;
                          update((d) => clean({ ...d, scenario: v ? { ...(d.scenario ?? {}), name: v } : undefined }));
                        }}
                      />
                      <TextInput
                        label="Required state"
                        placeholder="Started"
                        disabled={!draft.scenario?.name}
                        value={draft.scenario?.requiredState ?? ''}
                        onChange={(e) => {
                          const v = e.currentTarget.value;
                          update((d) => (d.scenario ? { ...d, scenario: clean({ ...d.scenario, requiredState: v || undefined }) } : d));
                        }}
                      />
                      <TextInput
                        label="New state"
                        disabled={!draft.scenario?.name}
                        value={draft.scenario?.newState ?? ''}
                        onChange={(e) => {
                          const v = e.currentTarget.value;
                          update((d) => (d.scenario ? { ...d, scenario: clean({ ...d.scenario, newState: v || undefined }) } : d));
                        }}
                      />
                    </div>
                  </div>
                  <Text size="xs" c="dimmed">
                    Callbacks, Kafka messages, response sequences, proxying and other advanced fields are kept as they are; edit them in the JSON tab.
                  </Text>
                </Stack>
              </Fieldset>
            </SectionCard>
          </Stack>
        </Tabs.Panel>

        <Tabs.Panel value="json" pt="md">
          <Stack gap="xs">
            <Text size="xs" c="dimmed">
              {stubSchema ? 'Validated against the Stub schema from the server API description. Ctrl+Space completes property names; hover a key for its description.' : 'The server API description is not available, so only JSON syntax is checked here.'}
            </Text>
            <CodeEditor
              value={jsonText}
              onChange={onJsonChange}
              readOnly={readOnly}
              schema={stubSchema}
              minHeight={420}
              maxHeight={900}
              label="Stub JSON"
              onDiagnostics={setLintCount}
              testId="stub-json-editor"
            />
            {jsonError && (
              <Alert color="red" role="alert">
                {jsonError}
              </Alert>
            )}
          </Stack>
        </Tabs.Panel>

        <Tabs.Panel value="test" pt="md">
          <Paper withBorder p="md" radius="md">
            <Text size="sm" c="dimmed" mb="sm">
              Sends a sample request to the mock (data plane) port. {isNew || !existing.data ? 'Save the stub first so the server can match it.' : 'Unsaved changes are not active until you save.'}
            </Text>
            {sample && <TestRequestPanel initial={sample} />}
          </Paper>
        </Tabs.Panel>
      </Tabs>
      {save.isError && (
        <Alert color="red" title="Save failed" role="alert">
          {errorMessage(save.error)}
        </Alert>
      )}

      {canEdit && (
        <div className={classes.bar} data-dirty={dirty || undefined}>
          <Group gap="xs" wrap="wrap" aria-live="polite" className={classes.barStatus}>
            {validationBadge}
            {fieldErrors > 0 && (
              <ToneBadge tone="danger">
                {fieldErrors} field{fieldErrors === 1 ? '' : 's'} to fix
              </ToneBadge>
            )}
            {dirty ? (
              <Text size="sm" c="dimmed" span className={classes.dirty}>
                Unsaved changes
              </Text>
            ) : save.isSuccess && !isNew ? (
              <Text size="sm" c="dimmed" role="status" span>
                Saved.
              </Text>
            ) : null}
          </Group>
          <Group gap="xs" wrap="nowrap">
            {!isNew && dirty && baseline && (
              <Button
                variant="subtle"
                color="gray"
                leftSection={<IconArrowBackUp size={16} />}
                onClick={() => {
                  const d = JSON.parse(baseline) as Stub;
                  const t = editorTexts(d);
                  setDraft(d);
                  setRespBodyText(t.resp);
                  setReqBodyText(t.req);
                  setRespBodyError(null);
                  setReqBodyError(null);
                }}
              >
                Discard
              </Button>
            )}
            <Tooltip label={<>Save <Kbd size="xs">Ctrl</Kbd> + <Kbd size="xs">S</Kbd></>}>
              <Button leftSection={<IconDeviceFloppy size={16} />} onClick={doSave} loading={save.isPending} data-testid="save-stub">
                {isNew ? 'Create stub' : 'Save'}
              </Button>
            </Tooltip>
          </Group>
        </div>
      )}
    </Stack>
  );
}
