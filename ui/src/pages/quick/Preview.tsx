import { Alert, Anchor, Badge, Button, Group, Paper, SegmentedControl, Stack, Tabs, Text, Textarea, TextInput } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { IconAlertTriangle, IconCheck, IconCode, IconExternalLink, IconLoader2, IconPlayerPlay, IconPlus, IconRefresh, IconSend, IconX } from '@tabler/icons-react';
import { useQuery, useQueryClient } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { Link } from 'react-router';
import { client, unwrap } from '../../api/client';
import { errorMessage } from '../../api/http';
import { notifyError, notifySuccess } from '../../api/mutations';
import { defaultDataPlaneBase, storeDataPlaneBase, storedDataPlaneBase, useUiConfig } from '../../api/uiConfig';
import { Permitted } from '../../components/Permitted';
import { CodeEditor } from '../../components/CodeEditor';
import { BodyView, Copy, MethodBadge, StatusBadge } from '../../components/common';
import { headerLines, joinUrl, parseHeaderLines, type SampleRequest } from '../../lib/curl';
import { docsUrl } from '../../lib/docs';
import { pretty } from '../../lib/format';
import { CODE_LANGS, codeSample, type CodeLang, type Issue, type QuickStub } from '../../lib/quickMock';
import classes from '../QuickMock.module.css';

export type Created = Partial<Record<QuickStub['role'], string>>;

interface ValidateResult {
  valid: boolean;
  error?: string;
}

/** Saves the stubs: creates new roles, replaces those created before and removes roles the form no longer has. */
async function saveStubs(stubs: QuickStub[], created: Created): Promise<Created> {
  const next: Created = {};
  const made: string[] = [];
  try {
    for (const s of stubs) {
      const id = created[s.role];
      if (id) {
        await unwrap(client.PUT('/__admin/stubs/{id}', { params: { path: { id } }, body: { ...s.stub, id } }));
        next[s.role] = id;
      } else {
        const saved = await unwrap(client.POST('/__admin/stubs', { body: s.stub }));
        if (saved.id) {
          made.push(saved.id);
          next[s.role] = saved.id;
        }
      }
    }
  } catch (e) {
    for (const id of made) await client.DELETE('/__admin/stubs/{id}', { params: { path: { id } } }).catch(() => undefined);
    throw e;
  }
  for (const [role, id] of Object.entries(created)) {
    if (id && !next[role as QuickStub['role']]) await unwrap(client.DELETE('/__admin/stubs/{id}', { params: { path: { id } } }));
  }
  return next;
}

export function PreviewPanel({
  stubs,
  issues,
  space,
  created,
  onCreated,
  onReset,
  sample,
  cors,
}: {
  stubs: QuickStub[];
  issues: Issue[];
  space: string | null;
  created: Created | null;
  onCreated: (c: Created) => void;
  onReset: () => void;
  sample: SampleRequest;
  cors: boolean;
}) {
  const qc = useQueryClient();
  const [tab, setTab] = useState<string>('preview');
  const [shown, setShown] = useState<QuickStub['role']>('main');
  const [busy, setBusy] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const current = stubs.find((s) => s.role === shown) ?? stubs[0];
  const json = current ? pretty(current.stub) : '';

  useEffect(() => {
    if (!stubs.some((s) => s.role === shown)) setShown('main');
  }, [stubs, shown]);

  const [debounced] = useDebouncedValue(issues.length ? null : stubs, 500);
  const validation = useQuery({
    queryKey: ['quick-validate', debounced],
    queryFn: () =>
      Promise.all(
        (debounced ?? []).map(async (s) => ({ role: s.role, title: s.title, result: (await unwrap(client.POST('/__admin/stubs/validate', { body: s.stub }))) as ValidateResult })),
      ),
    enabled: !!debounced,
    retry: false,
    staleTime: Infinity,
  });
  const rejected = validation.data?.filter((v) => !v.result.valid) ?? [];
  const serverOk = !!validation.data && rejected.length === 0 && !validation.isFetching;

  const save = async () => {
    setBusy(true);
    setSaveError(null);
    try {
      const next = await saveStubs(stubs, created ?? {});
      await Promise.all([qc.invalidateQueries({ queryKey: ['stubs'] }), qc.invalidateQueries({ queryKey: ['info'] })]);
      notifySuccess(created ? 'Mock updated' : `Mock created: ${String(stubs.length)} stub${stubs.length === 1 ? '' : 's'}`);
      onCreated(next);
      setTab('try');
    } catch (e) {
      setSaveError(errorMessage(e));
      notifyError(e, 'Could not save the mock');
    } finally {
      setBusy(false);
    }
  };

  const endpoint = stubs.length > 1 ? `${String(stubs.length)} × POST /__admin/stubs` : 'POST /__admin/stubs';

  return (
    <Paper withBorder radius="md" className={classes.preview} data-testid="quick-preview">
      <Tabs value={tab} onChange={(v) => v && setTab(v)} keepMounted={false}>
        <Tabs.List className={classes.previewTabs}>
          <Tabs.Tab value="preview" leftSection={<IconCode size={16} />}>
            Stub JSON
          </Tabs.Tab>
          <Tabs.Tab value="try" leftSection={<IconPlayerPlay size={16} />} data-testid="tab-try">
            Try it
          </Tabs.Tab>
        </Tabs.List>

        <Tabs.Panel value="preview" p="md">
          <Stack gap="sm">
            <Group justify="space-between" wrap="wrap" gap="xs">
              <Text size="xs" c="dimmed" className="mono">
                {endpoint}
                {space ? ` (space ${space})` : ''}
              </Text>
              <Copy value={json} label="Copy stub JSON" />
            </Group>
            {stubs.length > 1 && (
              <SegmentedControl
                size="xs"
                value={current?.role ?? 'main'}
                onChange={(v) => setShown(v)}
                data={stubs.map((s) => ({ value: s.role, label: s.title }))}
                aria-label="Stub to show"
                fullWidth
              />
            )}
            <CodeEditor value={json} language="json" readOnly minHeight={160} maxHeight={420} label="Stub JSON preview" testId="quick-json" />
            {stubs.length > 1 && (
              <Text size="xs" c="dimmed">
                {stubs.some((s) => s.role === 'auth') && 'The 401 fallback has a higher priority number, so it answers only when the credentials do not match. '}
                {stubs.some((s) => s.role === 'preflight') && 'The preflight stub answers the OPTIONS request a browser sends first.'}
              </Text>
            )}
            <div aria-live="polite">
              {issues.length > 0 ? (
                <Alert color="yellow" icon={<IconAlertTriangle size={18} />} title={`${String(issues.length)} thing${issues.length === 1 ? '' : 's'} to fix`} p="xs" data-testid="quick-issues">
                  <Stack gap={2}>
                    {issues.map((i, n) => (
                      <Text key={n} size="xs">
                        {i.message}
                      </Text>
                    ))}
                  </Stack>
                </Alert>
              ) : validation.isFetching || (!validation.data && !validation.isError) ? (
                <Badge color="gray" variant="light" tt="none" leftSection={<IconLoader2 size={12} className={classes.spin} />}>
                  checking with the server…
                </Badge>
              ) : rejected.length || validation.isError ? (
                <Alert color="red" icon={<IconX size={18} />} title="The server rejects this" p="xs" role="alert">
                  {validation.isError
                    ? errorMessage(validation.error)
                    : rejected.map((r) => (
                        <Text key={r.role} size="xs">
                          {stubs.length > 1 ? `${r.title}: ` : ''}
                          {r.result.error}
                        </Text>
                      ))}
                </Alert>
              ) : serverOk ? (
                <Badge color="green" variant="light" tt="none" leftSection={<IconCheck size={12} />} data-testid="quick-valid">
                  the server accepts {stubs.length === 1 ? 'this stub' : `these ${String(stubs.length)} stubs`}
                </Badge>
              ) : null}
            </div>
            {saveError && (
              <Alert color="red" title="Not saved" role="alert" p="xs">
                {saveError}
              </Alert>
            )}
            {created && (
              <Alert color="green" icon={<IconCheck size={18} />} title="Mock is live" p="xs" data-testid="quick-created">
                <Stack gap={4}>
                  <Text size="xs">Changes in the form are not active until you update the mock.</Text>
                  <Group gap="xs" wrap="wrap">
                    {Object.entries(created).map(([role, id]) => (
                      <Anchor key={role} component={Link} to={`/stubs/${id}`} size="xs">
                        Open {stubs.find((s) => s.role === role)?.title.toLowerCase() ?? 'stub'} in the editor
                      </Anchor>
                    ))}
                  </Group>
                </Stack>
              </Alert>
            )}
            <Group justify="flex-end" gap="xs" wrap="wrap">
              {created && (
                <Button variant="default" leftSection={<IconPlus size={16} />} onClick={onReset}>
                  New mock
                </Button>
              )}
              <Permitted perm="stubs:write" where={space}>
                <Button
                  leftSection={created ? <IconRefresh size={16} /> : <IconSend size={16} />}
                  onClick={() => void save()}
                  loading={busy}
                  disabled={issues.length > 0 || rejected.length > 0}
                  data-testid="quick-create"
                >
                  {created ? 'Update mock' : 'Create mock'}
                </Button>
              </Permitted>
            </Group>
          </Stack>
        </Tabs.Panel>

        <Tabs.Panel value="try" p="md">
          <TryIt sample={sample} created={!!created} cors={cors} />
        </Tabs.Panel>
      </Tabs>
    </Paper>
  );
}

interface Result {
  status: number;
  headers: Record<string, string>;
  body: string;
  ms: number;
}

const FORBIDDEN_HEADERS = new Set(['host', 'content-length', 'connection', 'origin', 'referer', 'cookie', 'user-agent', 'accept-encoding']);

function TryIt({ sample, created, cors }: { sample: SampleRequest; created: boolean; cors: boolean }) {
  const cfg = useUiConfig();
  const [base, setBase] = useState<string>(() => storedDataPlaneBase() ?? '');
  const [target, setTarget] = useState(() => sample.path + (sample.query ? `?${sample.query}` : ''));
  const [headers, setHeaders] = useState(headerLines(sample.headers));
  const [body, setBody] = useState(sample.body);
  const [result, setResult] = useState<Result | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [lang, setLang] = useState<CodeLang>('curl');

  useEffect(() => {
    if (!base && cfg.data) setBase(defaultDataPlaneBase(cfg.data));
  }, [cfg.data, base]);

  const key = JSON.stringify(sample);
  useEffect(() => {
    const s = JSON.parse(key) as SampleRequest;
    setTarget(s.path + (s.query ? `?${s.query}` : ''));
    setHeaders(headerLines(s.headers));
    setBody(s.body);
  }, [key]);

  const q = target.indexOf('?');
  const req: SampleRequest = {
    method: sample.method,
    path: q >= 0 ? target.slice(0, q) : target,
    query: q >= 0 ? target.slice(q + 1) : '',
    headers: parseHeaderLines(headers),
    body: sample.method === 'GET' ? '' : body,
  };
  const shownBase = base || 'http://localhost:8080';
  const code = codeSample(lang, shownBase, req);

  const send = async () => {
    setBusy(true);
    setFailure(null);
    setResult(null);
    storeDataPlaneBase(base);
    const started = performance.now();
    try {
      const h = new Headers();
      for (const [k, v] of Object.entries(req.headers)) if (!FORBIDDEN_HEADERS.has(k.toLowerCase())) h.set(k, v);
      const res = await fetch(joinUrl(base, req.path, req.query), {
        method: req.method,
        headers: h,
        body: req.method === 'GET' ? null : req.body || null,
        mode: 'cors',
        credentials: 'omit',
        cache: 'no-store',
        redirect: 'manual',
      });
      const text = await res.text();
      const rh: Record<string, string> = {};
      res.headers.forEach((v, k) => {
        rh[k] = v;
      });
      setResult({ status: res.status, headers: rh, body: text, ms: Math.round(performance.now() - started) });
    } catch {
      setFailure(
        cors
          ? 'The browser could not complete the request: the data plane may be unreachable at this URL, the page security policy may not allow it, or a network fault answered. Use one of the code samples below instead.'
          : 'The browser blocked the request. Turn on "Browser access (CORS)" and update the mock, or use one of the code samples below.',
      );
    } finally {
      setBusy(false);
    }
  };

  return (
    <Stack gap="sm">
      {!created && (
        <Alert color="blue" p="xs" icon={<IconAlertTriangle size={18} />}>
          Create the mock first: FastMock answers only stubs it has saved.
        </Alert>
      )}
      <TextInput label="Mock base URL" description="Where the data plane is reachable from this browser" value={base} onChange={(e) => setBase(e.currentTarget.value)} data-testid="quick-base" size="xs" classNames={{ input: 'mono' }} />
      <Group gap="xs" wrap="nowrap" align="center">
        <MethodBadge method={req.method} />
        <TextInput value={target} onChange={(e) => setTarget(e.currentTarget.value)} aria-label="Path and query" className={classes.grow} classNames={{ input: 'mono' }} size="xs" data-testid="quick-target" />
      </Group>
      <Textarea label="Headers" description="One per line: Name: value" value={headers} onChange={(e) => setHeaders(e.currentTarget.value)} autosize minRows={1} maxRows={6} size="xs" classNames={{ input: 'mono' }} />
      {req.method !== 'GET' && <Textarea label="Body" value={body} onChange={(e) => setBody(e.currentTarget.value)} autosize minRows={2} maxRows={10} size="xs" classNames={{ input: 'mono' }} />}
      <Group>
        <Button leftSection={<IconSend size={16} />} onClick={() => void send()} loading={busy} data-testid="quick-send">
          Send request
        </Button>
      </Group>
      {failure && (
        <Alert color="yellow" title="Request blocked" role="alert" p="xs">
          {failure}
        </Alert>
      )}
      {result && (
        <Paper withBorder p="sm" radius="md" data-testid="quick-result">
          <Group gap="xs" mb="xs">
            <StatusBadge status={result.status} />
            <Badge variant="outline" color="gray" tt="none" className="tabular">
              {result.ms} ms
            </Badge>
            {result.headers['x-fastmock-trace-id'] && (
              <Text size="xs" c="dimmed" className="mono truncate">
                trace {result.headers['x-fastmock-trace-id']}
              </Text>
            )}
          </Group>
          <BodyView body={result.body} contentType={result.headers['content-type']} />
          {result.status === 404 && (
            <Text size="xs" c="dimmed" mt={6}>
              404 with &quot;no stub matched&quot; means the request missed the stub; see{' '}
              <Anchor component={Link} to="/near-misses" size="xs">
                Near misses
              </Anchor>
              .
            </Text>
          )}
        </Paper>
      )}
      <div>
        <Group justify="space-between" mb={6} wrap="wrap" gap="xs">
          <SegmentedControl size="xs" value={lang} onChange={(v) => setLang(v)} data={CODE_LANGS} aria-label="Code sample language" />
          <Copy value={code} label={`Copy ${CODE_LANGS.find((l) => l.value === lang)?.label ?? ''} code`} />
        </Group>
        <pre className={`pre ${classes.code}`} data-testid="quick-code" tabIndex={0} aria-label="Code sample">
          {code}
        </pre>
      </div>
      <Text size="xs" c="dimmed">
        Calls show up in the{' '}
        <Anchor component={Link} to="/requests" size="xs">
          request journal
        </Anchor>
        .{' '}
        <Anchor href={docsUrl('guide/request-matching')} target="_blank" rel="noopener noreferrer" size="xs">
          Matching rules <IconExternalLink size={11} aria-hidden />
        </Anchor>
      </Text>
    </Stack>
  );
}
