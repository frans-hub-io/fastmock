import { Badge, Drawer, Group, NumberInput, Paper, Select, Stack, Table, Text, TextInput } from '@mantine/core';
import { useDebouncedValue } from '@mantine/hooks';
import { useQuery } from '@tanstack/react-query';
import { useMemo, useState } from 'react';
import { client, unwrap } from '../api/client';
import type { AuditEvent } from '../api/types';
import { useCan } from '../auth/useAuth';
import { DiffView } from '../components/DiffView';
import { PageHeader, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { formatAbsolute, pretty } from '../lib/format';

function beforeAfter(detail: unknown): { before: unknown; after: unknown } | null {
  if (!detail || typeof detail !== 'object') return null;
  const d = detail as Record<string, unknown>;
  if ('before' in d || 'after' in d) return { before: d.before, after: d.after };
  return null;
}

interface Change {
  path: string;
  before?: unknown;
  after?: unknown;
}

function changesOf(detail: unknown): Change[] | null {
  if (!detail || typeof detail !== 'object') return null;
  const c = (detail as Record<string, unknown>).changes;
  if (!Array.isArray(c)) return null;
  return c.filter((x): x is Change => !!x && typeof x === 'object' && typeof (x as Change).path === 'string');
}

function show(v: unknown): string {
  if (v === undefined) return '—';
  return typeof v === 'string' ? v : JSON.stringify(v);
}

function summary(detail: unknown): string {
  if (!detail || typeof detail !== 'object') return typeof detail === 'string' ? detail : JSON.stringify(detail ?? '');
  const d = { ...(detail as Record<string, unknown>) };
  delete d.before;
  delete d.after;
  delete d.changes;
  const s = JSON.stringify(d);
  return s.length > 140 ? `${s.slice(0, 137)}...` : s;
}

export default function Audit() {
  const isAdmin = useCan('admin');
  const [limit, setLimit] = useState(200);
  const [principal, setPrincipal] = useState('');
  const [action, setAction] = useState<string | null>(null);
  const [text, setText] = useState('');
  const [dPrincipal] = useDebouncedValue(principal, 300);
  const [selected, setSelected] = useState<AuditEvent | null>(null);
  const query = { limit, principal: dPrincipal || undefined, action: action ?? undefined };
  const q = useQuery({
    queryKey: ['audit', query],
    queryFn: () => unwrap(client.GET('/__admin/audit', { params: { query } })),
    enabled: isAdmin,
  });
  const events = useMemo(
    () =>
      (q.data?.events ?? []).filter(
        (e) =>
          (!dPrincipal || e.principal.includes(dPrincipal)) &&
          (!action || e.action === action) &&
          (!text || JSON.stringify(e.detail).toLowerCase().includes(text.toLowerCase())),
      ),
    [q.data, dPrincipal, action, text],
  );
  const actions = useMemo(() => [...new Set((q.data?.events ?? []).map((e) => e.action))].sort(), [q.data]);

  if (!isAdmin) return <ErrorState error={new Error('The audit log needs the admin role.')} />;
  const ba = selected ? beforeAfter(selected.detail) : null;
  const changes = selected ? changesOf(selected.detail) : null;

  return (
    <Stack gap="md">
      <PageHeader title="Audit log" description="Every change made through the admin API, who made it and when." />
      <Paper withBorder p="sm" radius="md">
        <Group gap="sm" wrap="wrap">
          <TextInput placeholder="Principal" value={principal} onChange={(e) => setPrincipal(e.currentTarget.value)} aria-label="Principal" />
          <Select placeholder="Any action" data={actions} value={action} onChange={setAction} clearable searchable aria-label="Action" w={220} />
          <TextInput placeholder="Detail contains" value={text} onChange={(e) => setText(e.currentTarget.value)} aria-label="Detail contains" />
          <NumberInput value={limit} onChange={(v) => setLimit(typeof v === 'number' ? v : 200)} min={1} max={10000} w={110} aria-label="Limit" />
        </Group>
      </Paper>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !events.length ? (
        <EmptyState title="No audit events" />
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm" highlightOnHover miw={720}>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>When</Table.Th>
                  <Table.Th>Principal</Table.Th>
                  <Table.Th>Action</Table.Th>
                  <Table.Th>Detail</Table.Th>
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {events.map((e) => (
                  <Table.Tr key={e.id} className="clickable-row" tabIndex={0} onClick={() => setSelected(e)} onKeyDown={(k) => k.key === 'Enter' && setSelected(e)}>
                    <Table.Td>
                      <Time at={e.at} />
                    </Table.Td>
                    <Table.Td>
                      {e.principal}{' '}
                      <Badge size="xs" variant="light">
                        {e.role}
                      </Badge>
                    </Table.Td>
                    <Table.Td className="mono">{e.action}</Table.Td>
                    <Table.Td className="mono" maw={480}>
                      <Text size="xs" className="truncate">
                        {summary(e.detail)}
                      </Text>
                    </Table.Td>
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
      <Drawer opened={!!selected} onClose={() => setSelected(null)} position="right" size="xl" title={selected?.action}>
        {selected && (
          <Stack gap="sm">
            <Text size="sm">
              {formatAbsolute(selected.at)} · {selected.principal} ({selected.role})
            </Text>
            {changes && (
              <Table fz="sm" withTableBorder data-testid="audit-changes">
                <Table.Thead>
                  <Table.Tr>
                    <Table.Th>Field</Table.Th>
                    <Table.Th>Before</Table.Th>
                    <Table.Th>After</Table.Th>
                  </Table.Tr>
                </Table.Thead>
                <Table.Tbody>
                  {changes.map((c, i) => (
                    <Table.Tr key={i}>
                      <Table.Td className="mono">{c.path}</Table.Td>
                      <Table.Td className="mono diff-del" style={{ wordBreak: 'break-all' }}>
                        {show(c.before)}
                      </Table.Td>
                      <Table.Td className="mono diff-add" style={{ wordBreak: 'break-all' }}>
                        {show(c.after)}
                      </Table.Td>
                    </Table.Tr>
                  ))}
                </Table.Tbody>
              </Table>
            )}
            {ba ? <DiffView left={ba.before} right={ba.after} leftLabel="before" rightLabel="after" /> : !changes && <pre className="pre">{pretty(selected.detail)}</pre>}
            {(ba ?? changes) && (
              <>
                <Text size="sm" fw={600}>
                  Other details
                </Text>
                <pre className="pre">{summary(selected.detail)}</pre>
              </>
            )}
          </Stack>
        )}
      </Drawer>
    </Stack>
  );
}
