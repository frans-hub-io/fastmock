import { ActionIcon, Alert, Badge, Button, Group, List, NumberInput, Paper, SegmentedControl, Select, Stack, Table, Text, TextInput, Title } from '@mantine/core';
import { IconPlus, IconTrash } from '@tabler/icons-react';
import { useMutation } from '@tanstack/react-query';
import { useState } from 'react';
import { useComponentSchema } from '../api/features';
import { client, unwrap, unwrapAllowing } from '../api/client';
import type { RequestPattern } from '../api/types';
import { CodeEditor } from '../components/CodeEditor';
import { MethodBadge, PageHeader, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState } from '../components/States';
import { pretty } from '../lib/format';
import { setUrl, URL_KIND_LABELS, urlKind, urlValue, type UrlKind } from '../lib/stubs';

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

function PatternEditor({ value, onChange, label }: { value: RequestPattern; onChange: (v: RequestPattern) => void; label: string }) {
  const schema = useComponentSchema('RequestPattern');
  const [text, setText] = useState(pretty(value));
  const kind = urlKind(value);
  const set = (p: RequestPattern) => {
    onChange(p);
    setText(pretty(p));
  };
  return (
    <Stack gap="xs">
      <Group gap="xs" wrap="wrap" align="flex-end">
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
          w={110}
          allowDeselect={false}
        />
        <Select
          label="URL match"
          data={Object.entries(URL_KIND_LABELS).map(([v, l]) => ({ value: v, label: l }))}
          value={kind}
          onChange={(v) => v && set(setUrl(value, v as UrlKind, urlValue(value)))}
          w={180}
          allowDeselect={false}
        />
        {kind !== 'any' && (
          <TextInput label="URL" value={urlValue(value)} onChange={(e) => set(setUrl(value, kind, e.currentTarget.value))} style={{ flex: 1 }} className="mono" data-testid="verify-url" />
        )}
      </Group>
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
            if (v && typeof v === 'object' && !Array.isArray(v)) onChange(v);
          } catch {
            /* keep last valid */
          }
        }}
      />
    </Stack>
  );
}

export default function Verify() {
  const [mode, setMode] = useState<Mode>('count');
  const [pattern, setPattern] = useState<RequestPattern>({ method: 'GET', urlPath: '/hello' });
  const [sequence, setSequence] = useState<RequestPattern[]>([{ method: 'POST', urlPath: '/login' }, { method: 'GET', urlPath: '/profile' }]);
  const [expect, setExpect] = useState<'count' | 'atLeast' | 'atMost'>('atLeast');
  const [n, setN] = useState<number>(1);
  const [source, setSource] = useState('');

  const find = useMutation({
    mutationFn: () => unwrap(client.POST('/__admin/requests/find', { body: pattern, params: { query: { limit: 50, source: source || undefined } } })),
  });
  const verify = useMutation({
    mutationFn: () => unwrapAllowing<VerifyResult>(client.POST('/__admin/verify', { body: { request: pattern, [expect]: n, ...(source ? { source } : {}) } }), 422),
  });
  const seq = useMutation({
    mutationFn: () => unwrapAllowing<VerifyResult>(client.POST('/__admin/verify/sequence', { body: { requests: sequence, ...(source ? { source } : {}) } }), 422),
  });

  const run = () => {
    if (mode === 'count') find.mutate();
    else if (mode === 'verify') verify.mutate();
    else seq.mutate();
  };
  const active = mode === 'count' ? find : mode === 'verify' ? verify : seq;
  const vr = mode === 'verify' ? verify.data : mode === 'sequence' ? seq.data : undefined;

  return (
    <Stack gap="md">
      <PageHeader title="Verify" description="Check the request journal against a request pattern: count matches, assert how many arrived, or assert an ordered sequence." />
      <Paper withBorder p="md" radius="md">
        <Stack gap="md">
          <Group justify="space-between" wrap="wrap">
            <SegmentedControl
              value={mode}
              onChange={(v) => setMode(v)}
              data={[
                { label: 'Count', value: 'count' },
                { label: 'Verify', value: 'verify' },
                { label: 'Sequence', value: 'sequence' },
              ]}
              aria-label="Verification mode"
            />
            <TextInput placeholder="source (optional)" value={source} onChange={(e) => setSource(e.currentTarget.value)} aria-label="Source" w={200} />
          </Group>
          {mode !== 'sequence' ? (
            <PatternEditor value={pattern} onChange={setPattern} label="Request pattern" />
          ) : (
            <Stack gap="md">
              {sequence.map((p, i) => (
                <Paper key={i} withBorder p="sm" radius="md">
                  <Group justify="space-between" mb="xs">
                    <Text fw={600} size="sm">
                      Step {i + 1}
                    </Text>
                    <ActionIcon variant="subtle" color="red" aria-label={`Remove step ${i + 1}`} onClick={() => setSequence(sequence.filter((_, j) => j !== i))} disabled={sequence.length <= 1}>
                      <IconTrash size={16} />
                    </ActionIcon>
                  </Group>
                  <PatternEditor value={p} onChange={(v) => setSequence(sequence.map((x, j) => (j === i ? v : x)))} label={`Step ${i + 1} pattern`} />
                </Paper>
              ))}
              <Group>
                <Button variant="light" leftSection={<IconPlus size={16} />} onClick={() => setSequence([...sequence, { method: 'GET', urlPath: '/' }])} disabled={sequence.length >= 50}>
                  Add step
                </Button>
              </Group>
            </Stack>
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
          <Group>
            <Button onClick={run} loading={active.isPending} data-testid="run-verify">
              {mode === 'count' ? 'Count matches' : mode === 'verify' ? 'Verify' : 'Verify sequence'}
            </Button>
          </Group>
        </Stack>
      </Paper>

      {active.isError && <ErrorState error={active.error} />}

      {mode === 'count' && find.data && (
        <Paper withBorder p="md" radius="md">
          <Title order={3} size="h5" mb="sm">
            <span data-testid="verify-count">{find.data.total}</span> matching request{find.data.total === 1 ? '' : 's'}
          </Title>
          {find.data.requests.length ? (
            <div className="table-scroll">
              <Table fz="sm">
                <Table.Tbody>
                  {find.data.requests.map((r) => (
                    <Table.Tr key={r.id}>
                      <Table.Td>
                        <Time at={r.at} />
                      </Table.Td>
                      <Table.Td>
                        <MethodBadge method={r.method} />
                      </Table.Td>
                      <Table.Td className="mono">{r.path}</Table.Td>
                      <Table.Td>
                        <StatusBadge status={r.status} />
                      </Table.Td>
                    </Table.Tr>
                  ))}
                </Table.Tbody>
              </Table>
            </div>
          ) : (
            <EmptyState title="Nothing matched" />
          )}
        </Paper>
      )}

      {vr && (
        <Alert color={vr.verified ? 'green' : 'red'} title={vr.verified ? 'Verified' : 'Not verified'} role="status" data-testid="verify-result">
          <Stack gap={6}>
            {vr.expected && (
              <Text size="sm">
                Expected {vr.expected}; found {vr.count ?? vr.matched ?? 0}.
              </Text>
            )}
            {mode === 'sequence' && (
              <Text size="sm">
                Matched {vr.matched ?? 0} of {sequence.length} steps{vr.requestIds?.length ? ` (journal ids ${vr.requestIds.join(', ')})` : ''}.
              </Text>
            )}
            {vr.error && <Text size="sm">{vr.error}</Text>}
            {!!vr.details?.length && (
              <List size="sm">
                {vr.details.map((d, i) => (
                  <List.Item key={i}>{d}</List.Item>
                ))}
              </List>
            )}
            {!!vr.nearMisses?.length && (
              <>
                <Text size="sm" fw={600}>
                  Closest requests
                </Text>
                {vr.nearMisses.map((m) => (
                  <Group key={m.requestId} gap="xs" wrap="nowrap" align="flex-start">
                    <Badge size="sm" variant="light">
                      {Math.round(m.score * 100)}%
                    </Badge>
                    <MethodBadge method={m.method} />
                    <Text size="sm" className="mono">
                      {m.path}
                    </Text>
                    <Text size="xs" c="dimmed">
                      {m.mismatches.join('; ')}
                    </Text>
                  </Group>
                ))}
              </>
            )}
          </Stack>
        </Alert>
      )}
    </Stack>
  );
}
