import { ActionIcon, Button, Group, NumberInput, SegmentedControl, Select, Stack, Table, Text, TextInput, Title, Tooltip } from '@mantine/core';
import { IconArrowDown, IconCircleCheck, IconCircleX, IconListSearch, IconPlayerPlay, IconPlus, IconShieldCheck, IconTrash } from '@tabler/icons-react';
import { useMutation } from '@tanstack/react-query';
import { useState } from 'react';
import { Link } from 'react-router';
import { useSpaceQuery } from '../auth/useAuth';
import { useComponentSchema } from '../api/features';
import { client, unwrap, unwrapAllowing } from '../api/client';
import type { RequestPattern } from '../api/types';
import { CodeEditor } from '../components/CodeEditor';
import { DataTable, MethodBadge, Meter, PageHeader, PathText, SectionCard, StatStrip, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { pretty } from '../lib/format';
import { setUrl, URL_KIND_LABELS, urlKind, urlValue, type UrlKind } from '../lib/stubs';
import classes from './Verify.module.css';

type Mode = 'count' | 'verify' | 'sequence';

interface VerifyResult {
  verified: boolean;
  count?: number;
  expected?: string;
  error?: string | null;
  details?: string[];
  nearMisses?: { requestId: number; method: string; path: string; score: number; mismatches: string[] }[];
  matched?: number;
  requestIds?: number[];
}

const MODES: { value: Mode; label: string; help: string }[] = [
  { value: 'count', label: 'Count', help: 'List the journal entries that match a pattern.' },
  { value: 'verify', label: 'Verify', help: 'Assert how many requests matching a pattern arrived.' },
  { value: 'sequence', label: 'Sequence', help: 'Assert that requests arrived in this order, other requests in between allowed.' },
];

function PatternEditor({ value, onChange, label }: { value: RequestPattern; onChange: (v: RequestPattern) => void; label: string }) {
  const schema = useComponentSchema('RequestPattern');
  const [text, setText] = useState(pretty(value));
  const [jsonError, setJsonError] = useState<string | null>(null);
  const kind = urlKind(value);
  const set = (p: RequestPattern) => {
    onChange(p);
    setText(pretty(p));
    setJsonError(null);
  };
  return (
    <Stack gap="xs">
      <div className={classes.pattern}>
        <Select
          label="Method"
          data={['ANY', 'GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS']}
          value={(value.method ?? 'ANY').toUpperCase()}
          onChange={(v) => {
            const next = { ...value };
            if (!v || v === 'ANY') delete next.method;
            else next.method = v;
            set(next);
          }}
          allowDeselect={false}
        />
        <Select label="URL match" data={Object.entries(URL_KIND_LABELS).map(([v, l]) => ({ value: v, label: l }))} value={kind} onChange={(v) => v && set(setUrl(value, v as UrlKind, urlValue(value)))} allowDeselect={false} />
        {kind !== 'any' && <TextInput label="URL" value={urlValue(value)} onChange={(e) => set(setUrl(value, kind, e.currentTarget.value))} classNames={{ input: 'mono' }} className={classes.url} data-testid="verify-url" />}
      </div>
      <CodeEditor
        value={text}
        label={label}
        minHeight={100}
        maxHeight={300}
        schema={schema}
        onChange={(t) => {
          setText(t);
          try {
            const v = JSON.parse(t) as unknown;
            if (v && typeof v === 'object' && !Array.isArray(v)) {
              onChange(v);
              setJsonError(null);
            } else setJsonError('The pattern must be a JSON object');
          } catch (e) {
            setJsonError(e instanceof Error ? e.message : String(e));
          }
        }}
      />
      {jsonError && (
        <Text size="xs" c="red" role="alert">
          {jsonError}; the last valid pattern is used.
        </Text>
      )}
    </Stack>
  );
}

function Outcome({ vr, mode, steps }: { vr: VerifyResult; mode: Mode; steps: number }) {
  return (
    <div className={classes.outcome} data-verified={vr.verified || undefined} role="status" data-testid="verify-result">
      <Group gap="sm" wrap="nowrap" align="flex-start">
        <span className={classes.outcomeIcon} aria-hidden>
          {vr.verified ? <IconCircleCheck size={26} /> : <IconCircleX size={26} />}
        </span>
        <Stack gap={4} miw={0}>
          <Title order={3} size="h4">
            {vr.verified ? 'Verified' : 'Not verified'}
          </Title>
          {vr.expected && (
            <Text size="sm">
              Expected {vr.expected}; found {vr.count ?? vr.matched ?? 0}.
            </Text>
          )}
          {mode === 'sequence' && (
            <Text size="sm">
              Matched {vr.matched ?? 0} of {steps} steps
              {vr.requestIds?.length ? (
                <>
                  {' '}
                  (journal ids{' '}
                  {vr.requestIds.map((id, i) => (
                    <span key={id}>
                      {i > 0 && ', '}
                      <Link to={`/requests?id=${String(id)}`}>{id}</Link>
                    </span>
                  ))}
                  )
                </>
              ) : null}
              .
            </Text>
          )}
          {vr.error && <Text size="sm">{vr.error}</Text>}
        </Stack>
      </Group>
      {mode === 'verify' && vr.count !== undefined && (
        <StatStrip
          items={[
            { label: 'Expected', value: vr.expected ?? '—' },
            { label: 'Found', value: vr.count, tone: vr.verified ? 'success' : 'danger', testId: 'verify-found' },
          ]}
        />
      )}
      {!!vr.details?.length && (
        <ul className="fm-error-list">
          {vr.details.map((d, i) => (
            <li key={i}>{d}</li>
          ))}
        </ul>
      )}
      {!!vr.nearMisses?.length && (
        <Stack gap={6}>
          <span className="fm-subhead">Closest requests</span>
          {vr.nearMisses.map((m) => (
            <div key={m.requestId} className={classes.miss}>
              <Group gap={8} wrap="nowrap" justify="space-between">
                <Group gap={8} wrap="nowrap" miw={0}>
                  <MethodBadge method={m.method} />
                  <PathText path={m.path} />
                </Group>
                <Meter value={m.score} label={`Similarity of request ${String(m.requestId)}`} tone="info" />
              </Group>
              {m.mismatches.length > 0 && (
                <Group gap={4} wrap="wrap" mt={6}>
                  {m.mismatches.map((x, i) => (
                    <span key={i} className="fm-chip">
                      {x}
                    </span>
                  ))}
                </Group>
              )}
            </div>
          ))}
        </Stack>
      )}
    </div>
  );
}

export default function Verify() {
  const [mode, setMode] = useState<Mode>('count');
  const [pattern, setPattern] = useState<RequestPattern>({ method: 'GET', urlPath: '/hello' });
  const [sequence, setSequence] = useState<RequestPattern[]>([{ method: 'POST', urlPath: '/login' }, { method: 'GET', urlPath: '/profile' }]);
  const [expect, setExpect] = useState<'count' | 'atLeast' | 'atMost'>('atLeast');
  const [n, setN] = useState<number>(1);
  const [source, setSource] = useState('');
  const spaceQ = useSpaceQuery();

  const find = useMutation({
    mutationFn: () => unwrap(client.POST('/__admin/requests/find', { body: pattern, params: { query: { limit: 50, source: source || undefined, ...spaceQ } } })),
  });
  const verify = useMutation({
    mutationFn: () => unwrapAllowing<VerifyResult>(client.POST('/__admin/verify', { body: { request: pattern, [expect]: n, ...(source ? { source } : {}) }, params: { query: spaceQ } }), 422),
  });
  const seq = useMutation({
    mutationFn: () => unwrapAllowing<VerifyResult>(client.POST('/__admin/verify/sequence', { body: { requests: sequence, ...(source ? { source } : {}) }, params: { query: spaceQ } }), 422),
  });

  const run = () => {
    if (mode === 'count') find.mutate();
    else if (mode === 'verify') verify.mutate();
    else seq.mutate();
  };
  const active = mode === 'count' ? find : mode === 'verify' ? verify : seq;
  const vr = mode === 'verify' ? verify.data : mode === 'sequence' ? seq.data : undefined;
  const help = MODES.find((m) => m.value === mode)?.help;
  const nothingYet = !active.isError && !active.isPending && (mode === 'count' ? !find.data : !vr);

  return (
    <Stack gap="md">
      <PageHeader
        title="Verify"
        description="Check the request journal against a request pattern: count matches, assert how many arrived, or assert an ordered sequence."
        actions={
          <Button component="a" href={docsUrl('guide/verification')} target="_blank" rel="noopener noreferrer" variant="default" leftSection={<IconShieldCheck size={16} />}>
            Verification guide
          </Button>
        }
      />
      <div className="fm-aside-col">
        <SectionCard title="Check" description={help} icon={<IconListSearch size={16} />}>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              run();
            }}
          >
            <Stack gap="md">
              <Group justify="space-between" wrap="wrap" gap="sm">
                <SegmentedControl value={mode} onChange={(v) => setMode(v)} data={MODES.map(({ value, label }) => ({ value, label }))} aria-label="Verification mode" />
                <TextInput placeholder="source (optional)" value={source} onChange={(e) => setSource(e.currentTarget.value)} aria-label="Source" w={{ base: '100%', sm: 200 }} />
              </Group>
              {mode !== 'sequence' ? (
                <PatternEditor value={pattern} onChange={setPattern} label="Request pattern" />
              ) : (
                <ol className={classes.steps}>
                  {sequence.map((p, i) => (
                    <li key={i} className={classes.step}>
                      <div className={classes.stepRail} aria-hidden>
                        <span className={classes.stepNo}>{i + 1}</span>
                        {i < sequence.length - 1 && <IconArrowDown size={14} className={classes.stepArrow} />}
                      </div>
                      <div className={classes.stepBody}>
                        <Group justify="space-between" mb="xs">
                          <Text fw={600} size="sm">
                            Step {i + 1}
                          </Text>
                          <Tooltip label="Remove step">
                            <ActionIcon variant="subtle" color="red" aria-label={`Remove step ${String(i + 1)}`} onClick={() => setSequence(sequence.filter((_, j) => j !== i))} disabled={sequence.length <= 1}>
                              <IconTrash size={16} />
                            </ActionIcon>
                          </Tooltip>
                        </Group>
                        <PatternEditor value={p} onChange={(v) => setSequence(sequence.map((x, j) => (j === i ? v : x)))} label={`Step ${String(i + 1)} pattern`} />
                      </div>
                    </li>
                  ))}
                  <li className={classes.addStep}>
                    <Button variant="light" leftSection={<IconPlus size={16} />} onClick={() => setSequence([...sequence, { method: 'GET', urlPath: '/' }])} disabled={sequence.length >= 50}>
                      Add step
                    </Button>
                  </li>
                </ol>
              )}
              {mode === 'verify' && (
                <Group gap="xs" align="flex-end">
                  <Select
                    label="Expectation"
                    data={[
                      { value: 'count', label: 'exactly' },
                      { value: 'atLeast', label: 'at least' },
                      { value: 'atMost', label: 'at most' },
                    ]}
                    value={expect}
                    onChange={(v) => v && setExpect(v)}
                    w={140}
                    allowDeselect={false}
                  />
                  <NumberInput label="Times" value={n} onChange={(v) => setN(typeof v === 'number' ? v : 0)} min={0} max={1000000} w={120} />
                </Group>
              )}
              <Group justify="flex-end">
                <Button type="submit" loading={active.isPending} data-testid="run-verify" leftSection={<IconPlayerPlay size={16} />}>
                  {mode === 'count' ? 'Count matches' : mode === 'verify' ? 'Verify' : 'Verify sequence'}
                </Button>
              </Group>
            </Stack>
          </form>
        </SectionCard>

        <div className="fm-sticky-col">
          {active.isError && <ErrorState error={active.error} />}
          {nothingYet && (
            <SectionCard title="Result" icon={<IconShieldCheck size={16} />}>
              <EmptyState compact title="Nothing checked yet" icon={<IconShieldCheck size={20} />}>
                Describe a request and run the check; the answer appears here.
              </EmptyState>
            </SectionCard>
          )}
          {mode === 'count' && find.data && (
            <SectionCard
              title={
                <>
                  <span data-testid="verify-count">{find.data.total}</span> matching request{find.data.total === 1 ? '' : 's'}
                </>
              }
              description={find.data.requests.length < find.data.total ? `Showing the latest ${String(find.data.requests.length)}` : 'From the request journal'}
              icon={<IconListSearch size={16} />}
            >
              {find.data.requests.length ? (
                <DataTable testId="verify-matches">
                  <Table.Tbody>
                    {find.data.requests.map((r) => (
                      <Table.Tr key={r.id}>
                        <Table.Td>
                          <Group gap={8} wrap="nowrap">
                            <MethodBadge method={r.method} />
                            <PathText path={r.path} query={r.query} />
                          </Group>
                        </Table.Td>
                        <Table.Td data-label="Status">
                          <StatusBadge status={r.status} />
                        </Table.Td>
                        <Table.Td data-label="When">
                          <Time at={r.at} />
                        </Table.Td>
                      </Table.Tr>
                    ))}
                  </Table.Tbody>
                </DataTable>
              ) : (
                <EmptyState compact title="Nothing matched" icon={<IconListSearch size={20} />}>
                  No journal entry matches the pattern. Check the method, the URL match and the source.
                </EmptyState>
              )}
            </SectionCard>
          )}
          {vr && <Outcome vr={vr} mode={mode} steps={sequence.length} />}
        </div>
      </div>
    </Stack>
  );
}
