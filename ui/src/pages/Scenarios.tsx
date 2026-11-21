import { ActionIcon, Button, Group, Stack, Table, Text, TextInput, Tooltip } from '@mantine/core';
import { IconArrowBackUp, IconChecklist, IconCheck, IconRefresh, IconRotate, IconSearch } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { client, unwrap } from '../api/client';
import { confirmAction, useApiMutation } from '../api/mutations';
import type { ScenarioState } from '../api/types';
import { useCan, useSpace, useSpaceChoice, useSpaceQuery } from '../auth/useAuth';
import { DataTable, DensityToggle, PageHeader, SectionCard } from '../components/common';
import { SpaceBadge, SpaceField } from '../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { useDensity } from '../lib/ui';

const STARTED = 'Started';
const MAX = 512;

function StatePill({ state }: { state: string }) {
  return (
    <span className={state === STARTED ? 'fm-chip' : 'fm-chip fm-chip-soft'} data-testid="scenario-state">
      {state}
    </span>
  );
}

function StateEditor({ s }: { s: ScenarioState }) {
  const [value, setValue] = useState(s.state);
  const set = useApiMutation<string>({
    fn: (state) => unwrap(client.PUT('/__admin/scenarios/{name}', { params: { path: { name: s.name }, query: s.space ? { space: s.space } : {} }, body: { state } })),
    invalidate: [['scenarios']],
    success: (_d, v) => `${s.name} → ${v}`,
  });
  const changed = !!value.trim() && value !== s.state;
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        if (changed) set.mutate(value);
      }}
    >
      <Group gap={6} wrap="nowrap" justify="flex-end">
        <TextInput size="xs" value={value} onChange={(e) => setValue(e.currentTarget.value)} aria-label={`New state for ${s.name}`} maxLength={MAX} classNames={{ input: 'mono' }} miw={0} style={{ flex: '1 1 140px' }} />
        <Tooltip label="Set this state">
          <ActionIcon type="submit" variant="light" size="md" disabled={!changed} loading={set.isPending} aria-label={`Set state of ${s.name}`}>
            <IconCheck size={16} />
          </ActionIcon>
        </Tooltip>
        <Tooltip label={`Reset to ${STARTED}`}>
          <ActionIcon
            variant="subtle"
            color="gray"
            size="md"
            onClick={() => {
              setValue(STARTED);
              set.mutate(STARTED);
            }}
            disabled={s.state === STARTED}
            aria-label={`Reset ${s.name}`}
          >
            <IconArrowBackUp size={16} />
          </ActionIcon>
        </Tooltip>
      </Group>
    </form>
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
  const [touched, setTouched] = useState(false);
  const [newSpace, setNewSpace] = useSpaceChoice('state:write');
  const [density, setDensity] = useDensity();
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
      setTouched(false);
    },
  });
  const all = q.data?.scenarios ?? [];
  const rows = all.filter((s) => !filter || s.name.toLowerCase().includes(filter.toLowerCase()));
  const moved = all.filter((s) => s.state !== STARTED).length;
  const nameError = touched && !newName.trim() ? 'Name the scenario' : undefined;
  const stateError = touched && !newState.trim() ? 'Name the state to move it to' : undefined;

  return (
    <Stack gap="md">
      <PageHeader
        title="Scenarios"
        description="State machines that stubs move through. A scenario with no stored state is in the Started state."
        meta={
          all.length > 0 && (
            <>
              <span className="fm-chip">{all.length} tracked</span>
              <span className="fm-chip fm-chip-soft">{moved} moved from Started</span>
            </>
          )
        }
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
                disabled={!all.length}
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
        <SectionCard title="Set a state" description="Move a scenario to any state, for example to start a test from the middle of a flow" icon={<IconChecklist size={16} />}>
          <form
            onSubmit={(e) => {
              e.preventDefault();
              setTouched(true);
              if (newName.trim() && newState.trim()) setOne.mutate({ name: newName.trim(), state: newState.trim() });
            }}
          >
            <Group gap="sm" align="flex-start" wrap="wrap">
              <SpaceField perm="state:write" value={newSpace} onChange={setNewSpace} />
              <TextInput label="Scenario" placeholder="order-42" value={newName} onChange={(e) => setNewName(e.currentTarget.value)} maxLength={MAX} error={nameError} classNames={{ input: 'mono' }} className="fm-grow" />
              <TextInput label="State" placeholder="paid" value={newState} onChange={(e) => setNewState(e.currentTarget.value)} maxLength={MAX} error={stateError} classNames={{ input: 'mono' }} className="fm-grow" />
              <Button type="submit" loading={setOne.isPending} mt={{ base: 0, sm: 25 }}>
                Set state
              </Button>
            </Group>
          </form>
        </SectionCard>
      )}
      <div className="fm-toolbar">
        <TextInput placeholder="Filter by name" leftSection={<IconSearch size={16} />} value={filter} onChange={(e) => setFilter(e.currentTarget.value)} aria-label="Filter scenarios" className="fm-grow" />
        <DensityToggle value={density} onChange={setDensity} />
      </div>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !rows.length ? (
        filter ? (
          <EmptyState
            title="No scenarios match the filter"
            icon={<IconSearch size={28} />}
            action={
              <Button variant="default" onClick={() => setFilter('')}>
                Clear filter
              </Button>
            }
          >
            Try another name.
          </EmptyState>
        ) : (
          <EmptyState title="No scenario has moved yet" icon={<IconChecklist size={30} />} docs={{ label: 'Scenarios and state', href: docsUrl('guide/scenarios-and-state') }}>
            Scenarios appear here once a stub changes their state{canEdit ? ', or when you set one above' : ''}. A stub joins a scenario with its scenario section.
          </EmptyState>
        )
      ) : (
        <DataTable
          testId="scenarios-table"
          density={density}
          miw={canEdit ? 760 : 480}
          footer={
            <Text size="xs" c="dimmed">
              {rows.length} scenario{rows.length === 1 ? '' : 's'} · refreshed every 5 seconds
            </Text>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Scenario</Table.Th>
              <Table.Th>Current state</Table.Th>
              {spaces.enabled && <Table.Th>Space</Table.Th>}
              {canEdit && <Table.Th w={320}>Change</Table.Th>}
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {rows.map((s) => (
              <Table.Tr key={`${s.space ?? ''}/${s.name}`} data-testid="scenario-row">
                <Table.Td>
                  <Text size="sm" fw={600} className="mono break-all">
                    {s.name}
                  </Text>
                </Table.Td>
                <Table.Td data-label="State">
                  <StatePill state={s.state} />
                </Table.Td>
                {spaces.enabled && (
                  <Table.Td data-label="Space">
                    <SpaceBadge space={s.space} />
                  </Table.Td>
                )}
                {canEdit && <Table.Td data-empty={can('state:write', s.space ?? null) ? undefined : true}>{can('state:write', s.space ?? null) && <StateEditor s={s} />}</Table.Td>}
              </Table.Tr>
            ))}
          </Table.Tbody>
        </DataTable>
      )}
    </Stack>
  );
}
