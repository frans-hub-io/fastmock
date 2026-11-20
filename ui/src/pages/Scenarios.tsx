import { Button, Group, Paper, Stack, Table, Text, TextInput } from '@mantine/core';
import { IconRefresh, IconRotate } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { ScenarioState } from '../api/types';
import { useCan, useSpace, useSpaceChoice, useSpaceQuery } from '../auth/useAuth';
import { PageHeader } from '../components/common';
import { SpaceBadge, SpaceField } from '../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../components/States';

function StateEditor({ s }: { s: ScenarioState }) {
  const [value, setValue] = useState(s.state);
  const set = useApiMutation<string>({
    fn: (state) => unwrap(client.PUT('/__admin/scenarios/{name}', { params: { path: { name: s.name }, query: s.space ? { space: s.space } : {} }, body: { state } })),
    invalidate: [['scenarios']],
    success: (_d, v) => `${s.name} → ${v}`,
  });
  return (
    <Group gap="xs" wrap="nowrap">
      <TextInput size="xs" value={value} onChange={(e) => setValue(e.currentTarget.value)} aria-label={`New state for ${s.name}`} maxLength={512} />
      <Button size="xs" variant="light" disabled={!value || value === s.state} loading={set.isPending} onClick={() => set.mutate(value)}>
        Set
      </Button>
      <Button size="xs" variant="subtle" onClick={() => set.mutate('Started')} disabled={s.state === 'Started'}>
        Reset
      </Button>
    </Group>
  );
}

export default function Scenarios() {
  const can = useCan();
  const canEdit = can('state:write');
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [filter, setFilter] = useState('');
  const [newName, setNewName] = useState('');
  const [newState, setNewState] = useState('');
  const [newSpace, setNewSpace] = useSpaceChoice('state:write');
  const q = useQuery({ queryKey: ['scenarios', spaceQ], queryFn: () => unwrap(client.GET('/__admin/scenarios', { params: { query: spaceQ } })), refetchInterval: 5000 });
  const resetAll = useApiMutation<void, { reset: number }>({
    fn: () => unwrap(client.DELETE('/__admin/scenarios', { params: { query: spaceQ } })),
    invalidate: [['scenarios']],
    success: (d) => `Reset ${d.reset} scenarios`,
  });
  const setOne = useApiMutation<{ name: string; state: string }>({
    fn: ({ name, state }) => unwrap(client.PUT('/__admin/scenarios/{name}', { params: { path: { name }, query: newSpace ? { space: newSpace } : {} }, body: { state } })),
    invalidate: [['scenarios']],
    success: (_d, v) => `${v.name} → ${v.state}`,
    onSuccess: () => {
      setNewName('');
      setNewState('');
    },
  });
  const rows = (q.data?.scenarios ?? []).filter((s) => !filter || s.name.toLowerCase().includes(filter.toLowerCase()));

  return (
    <Stack gap="md">
      <PageHeader
        title="Scenarios"
        description="State machines that stubs move through. A scenario with no stored state is in the Started state."
        actions={
          <>
            <Button variant="default" leftSection={<IconRefresh size={16} />} onClick={() => void q.refetch()}>
              Refresh
            </Button>
            {canEdit && (
              <Button
                color="red"
                variant="light"
                leftSection={<IconRotate size={16} />}
                onClick={() =>
                  confirmAction({ title: spaces.current ? `Reset all scenarios of space ${spaces.current}?` : 'Reset all scenarios?', message: 'Every scenario returns to the Started state.', confirm: 'Reset all', onConfirm: () => resetAll.mutate() })
                }
              >
                Reset all
              </Button>
            )}
          </>
        }
      />
      {canEdit && (
        <Paper withBorder p="sm" radius="md">
          <Group gap="xs" align="flex-end" wrap="wrap">
            <SpaceField perm="state:write" value={newSpace} onChange={setNewSpace} />
            <TextInput label="Scenario" placeholder="order-42" value={newName} onChange={(e) => setNewName(e.currentTarget.value)} maxLength={512} />
            <TextInput label="State" placeholder="paid" value={newState} onChange={(e) => setNewState(e.currentTarget.value)} maxLength={512} />
            <Button disabled={!newName || !newState} onClick={() => setOne.mutate({ name: newName, state: newState })} loading={setOne.isPending}>
              Set state
            </Button>
          </Group>
        </Paper>
      )}
      <TextInput placeholder="Filter by name" value={filter} onChange={(e) => setFilter(e.currentTarget.value)} aria-label="Filter scenarios" maw={360} />
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !rows.length ? (
        <EmptyState title="No scenario has moved yet">Scenarios appear here once a stub changes their state.</EmptyState>
      ) : (
        <Paper withBorder radius="md">
          <div className="table-scroll">
            <Table fz="sm" highlightOnHover>
              <Table.Thead>
                <Table.Tr>
                  <Table.Th>Scenario</Table.Th>
                  <Table.Th>Current state</Table.Th>
                  {spaces.enabled && <Table.Th>Space</Table.Th>}
                  {canEdit && <Table.Th>Change</Table.Th>}
                </Table.Tr>
              </Table.Thead>
              <Table.Tbody>
                {rows.map((s) => (
                  <Table.Tr key={`${s.space ?? ''}/${s.name}`}>
                    <Table.Td className="mono">{s.name}</Table.Td>
                    <Table.Td>
                      <Text fw={600} size="sm">
                        {s.state}
                      </Text>
                    </Table.Td>
                    {spaces.enabled && (
                      <Table.Td>
                        <SpaceBadge space={s.space} />
                      </Table.Td>
                    )}
                    {canEdit && <Table.Td>{can('state:write', s.space ?? null) && <StateEditor s={s} />}</Table.Td>}
                  </Table.Tr>
                ))}
              </Table.Tbody>
            </Table>
          </div>
        </Paper>
      )}
    </Stack>
  );
}
