import { ActionIcon, Alert, Button, Checkbox, Fieldset, Group, Modal, NumberInput, SegmentedControl, Select, Stack, Switch, Table, TagsInput, Text, Textarea, TextInput, Tooltip } from '@mantine/core';
import { useMediaQuery } from '@mantine/hooks';
import { IconEye, IconLock, IconPencil, IconPlugConnected, IconPlus, IconTrash } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { captureApi, type SourceView } from '../../api/capture';
import { confirmAction, useApiMutation } from '../../api/mutations';
import { useCan, useSpace, useSpaceChoice, useSpaceQuery } from '../../auth/useAuth';
import { DataTable, Time, ToneBadge } from '../../components/common';
import { Permitted } from '../../components/Permitted';
import { SpaceBadge, SpaceField } from '../../components/spaces';
import { EmptyState, ErrorState, LoadingState } from '../../components/States';
import { draftToSource, EMPTY_SOURCE, newSourceId, PROTOCOLS, sourceIssues, sourceTargets, sourceToDraft, stateTone, type SourceDraft } from '../../lib/capture';
import { ProtocolBadge } from './parts';

interface Editing {
  draft: SourceDraft;
  /** The space of an existing source; undefined for a new one (the space field decides). */
  space?: string | null;
  readOnly: boolean;
  fromConfig: boolean;
}

function SourceForm({ editing, maxMessages, onClose }: { editing: Editing; maxMessages: number; onClose: () => void }) {
  const phone = useMediaQuery('(max-width: 48em)');
  const [draft, setDraft] = useState(editing.draft);
  const [touched, setTouched] = useState(false);
  const [space, setSpace] = useSpaceChoice('messages:write');
  const isNew = !editing.draft.id;
  const issues = sourceIssues(draft, maxMessages);
  const show = (k: keyof typeof issues) => (touched ? issues[k] : undefined);
  const save = useApiMutation<void, SourceView>({
    fn: () => captureApi.putSource(draft.id ?? newSourceId(), draftToSource(draft), isNew ? space : editing.space),
    invalidate: [['capture', 'sources']],
    success: (s) => `Saved ${s.name}`,
    onSuccess: onClose,
  });
  const set = (patch: Partial<SourceDraft>) => setDraft((d) => ({ ...d, ...patch }));
  const title = isNew ? 'Add a source' : editing.readOnly ? draft.name : `Edit ${draft.name}`;

  return (
    <Modal opened onClose={onClose} title={title} size="lg" fullScreen={phone} data-testid="source-dialog">
      <form
        onSubmit={(e) => {
          e.preventDefault();
          setTouched(true);
          if (editing.readOnly || Object.keys(issues).length) return;
          save.mutate();
        }}
      >
        <Stack gap="sm">
          {editing.readOnly && (
            <Alert color="gray" icon={<IconLock size={18} />} p="xs" data-testid="source-read-only">
              {editing.fromConfig ? 'Defined in the server configuration (FASTMOCK_CAPTURE_SOURCES); change it there.' : 'Read-only: changing sources requires the messages:write permission.'}
            </Alert>
          )}
          <Fieldset variant="unstyled" disabled={editing.readOnly} p={0} m={0}>
            <Stack gap="sm">
              <TextInput
                label="Name"
                description="Lowercase letters, digits and . _ -; filters and matchers use it"
                placeholder="orders-events"
                value={draft.name}
                onChange={(e) => set({ name: e.currentTarget.value })}
                error={show('name')}
                withAsterisk
                classNames={{ input: 'mono' }}
                data-testid="source-name"
              />
              {isNew && <SpaceField perm="messages:write" value={space} onChange={setSpace} description="Where the source and its messages go" />}
              <div>
                <Text size="sm" fw={500} mb={4}>
                  Protocol
                </Text>
                <SegmentedControl
                  value={draft.protocol}
                  onChange={(v) => set({ protocol: v })}
                  data={PROTOCOLS}
                  aria-label="Protocol"
                  data-testid="source-protocol"
                />
              </div>
              {draft.protocol === 'kafka' && (
                <>
                  <TagsInput
                    label="Topics"
                    description="Press Enter after each topic"
                    placeholder="orders.created"
                    value={draft.topics}
                    onChange={(v) => set({ topics: v })}
                    error={show('topics')}
                    classNames={{ input: 'mono' }}
                    data-testid="source-topics"
                  />
                  <div>
                    <Text size="sm" fw={500} mb={4}>
                      Start from
                    </Text>
                    <SegmentedControl
                      value={draft.startFrom}
                      onChange={(v) => set({ startFrom: v === 'earliest' ? 'earliest' : 'latest' })}
                      data={[
                        { value: 'latest', label: 'New messages' },
                        { value: 'earliest', label: 'Earliest offset' },
                      ]}
                      aria-label="Start from"
                      data-testid="source-start-from"
                    />
                  </div>
                </>
              )}
              {draft.protocol === 'mqtt' && (
                <Group gap="sm" align="flex-start" grow wrap="wrap">
                  <TagsInput
                    label="Topic filters"
                    description="+ and # wildcards as whole levels"
                    placeholder="devices/+/telemetry"
                    value={draft.topics}
                    onChange={(v) => set({ topics: v })}
                    error={show('topics')}
                    classNames={{ input: 'mono' }}
                    miw={240}
                    data-testid="source-topics"
                  />
                  <Select
                    label="QoS"
                    description="Subscription quality of service"
                    data={['0', '1', '2']}
                    value={String(draft.qos)}
                    onChange={(v) => set({ qos: Number(v ?? 1) })}
                    allowDeselect={false}
                    maw={160}
                    data-testid="source-qos"
                  />
                </Group>
              )}
              {draft.protocol === 'amqp' && (
                <>
                  <TextInput
                    label="Exchange"
                    description="FastMock binds a queue of its own to it; the application's queues lose nothing"
                    placeholder="orders"
                    value={draft.exchange}
                    onChange={(e) => set({ exchange: e.currentTarget.value })}
                    error={show('exchange')}
                    withAsterisk
                    classNames={{ input: 'mono' }}
                    data-testid="source-exchange"
                  />
                  <TagsInput
                    label="Binding keys"
                    description="Empty: # (every message of a topic exchange)"
                    placeholder="order.*"
                    value={draft.bindingKeys}
                    onChange={(v) => set({ bindingKeys: v })}
                    error={show('bindingKeys')}
                    classNames={{ input: 'mono' }}
                    data-testid="source-binding-keys"
                  />
                </>
              )}
              <NumberInput
                label="Messages kept"
                description="The oldest are dropped first"
                placeholder={maxMessages ? `server limit, ${String(maxMessages)}` : 'server limit'}
                value={draft.maxMessages}
                onChange={(v) => set({ maxMessages: typeof v === 'number' ? v : '' })}
                min={1}
                max={maxMessages || undefined}
                allowDecimal={false}
                error={show('maxMessages')}
                maw={260}
                data-testid="source-max-messages"
              />
              <Textarea label="Description" value={draft.description} onChange={(e) => set({ description: e.currentTarget.value })} autosize minRows={2} error={show('description')} data-testid="source-description" />
              <Switch label="Enabled" description="A disabled source keeps its messages but does not subscribe" checked={draft.enabled} onChange={(e) => set({ enabled: e.currentTarget.checked })} data-testid="source-enabled" />
            </Stack>
          </Fieldset>
          <Group justify="flex-end" gap="xs">
            <Button variant="subtle" color="gray" onClick={onClose}>
              {editing.readOnly ? 'Close' : 'Cancel'}
            </Button>
            {!editing.readOnly && (
              <Button type="submit" loading={save.isPending} data-testid="source-save">
                {isNew ? 'Add source' : 'Save'}
              </Button>
            )}
          </Group>
        </Stack>
      </form>
    </Modal>
  );
}

function PurgeChoice({ onChange }: { onChange: (purge: boolean) => void }) {
  const [purge, setPurge] = useState(false);
  return (
    <Checkbox
      label="Also delete its stored messages"
      description="Otherwise they stay until the message limit or retention removes them"
      checked={purge}
      onChange={(e) => {
        setPurge(e.currentTarget.checked);
        onChange(e.currentTarget.checked);
      }}
      data-testid="source-purge"
    />
  );
}

function StateCell({ s }: { s: SourceView }) {
  const st = s.status;
  return (
    <Stack gap={2} miw={0}>
      <ToneBadge tone={stateTone(st.state)} testId="source-state">
        {st.state}
      </ToneBadge>
      {st.lastError && (
        <Text size="xs" c="red" className="break-all" data-testid="source-error">
          {st.lastError}
        </Text>
      )}
    </Stack>
  );
}

export function Sources() {
  const can = useCan();
  const spaces = useSpace();
  const spaceQ = useSpaceQuery();
  const [editing, setEditing] = useState<Editing | null>(null);
  const q = useQuery({ queryKey: ['capture', 'sources', spaceQ.space ?? null], queryFn: () => captureApi.sources(spaceQ.space), refetchInterval: 5000 });
  const del = useApiMutation<{ s: SourceView; purge: boolean }>({
    fn: ({ s, purge }) => captureApi.removeSource(s.id ?? '', purge),
    invalidate: [['capture']],
    success: (_d, { s }) => `Deleted ${s.name}`,
  });
  const list = q.data?.sources ?? [];
  const maxSources = q.data?.maxSources ?? 0;
  const maxMessages = q.data?.maxMessages ?? 0;
  const full = maxSources > 0 && list.length >= maxSources;
  const showSpace = spaces.enabled && !spaces.current;

  const openNew = () => setEditing({ draft: { ...EMPTY_SOURCE }, readOnly: false, fromConfig: false });
  const open = (s: SourceView) => {
    const fromConfig = !!s.fromConfig;
    setEditing({ draft: sourceToDraft(s), space: s.space ?? null, fromConfig, readOnly: fromConfig || !can('messages:write', s.space ?? null) });
  };
  const addButton = (
    <Permitted perm="messages:write">
      <Button leftSection={<IconPlus size={16} />} onClick={openNew} disabled={full} data-testid="source-add">
        Add source
      </Button>
    </Permitted>
  );

  return (
    <Stack gap="md">
      <Group justify="space-between" gap="sm" wrap="wrap">
        <Text size="sm" c="dimmed" data-testid="sources-limits">
          {q.data ? `${String(list.length)} of ${String(maxSources)} sources · at most ${String(maxMessages)} messages kept per source` : ' '}
        </Text>
        {full ? (
          <Tooltip label={`The server allows ${String(maxSources)} sources (FASTMOCK_CAPTURE_MAX_SOURCES)`} withArrow>
            <span>{addButton}</span>
          </Tooltip>
        ) : (
          addButton
        )}
      </Group>
      {q.isLoading ? (
        <LoadingState />
      ) : q.isError ? (
        <ErrorState error={q.error} onRetry={() => void q.refetch()} />
      ) : !list.length ? (
        <EmptyState
          title="No sources"
          icon={<IconPlugConnected size={30} />}
          action={
            can('messages:write') ? (
              <Button leftSection={<IconPlus size={16} />} onClick={openNew}>
                Add a source
              </Button>
            ) : undefined
          }
          testId="sources-empty"
        >
          A source subscribes to Kafka topics, MQTT topic filters or an AMQP exchange and stores what arrives, so you can inspect and assert on the messages your application publishes.
        </EmptyState>
      ) : (
        <DataTable
          testId="sources-table"
          miw={980}
          footer={
            <Text size="xs" c="dimmed">
              Status refreshed every 5 seconds
            </Text>
          }
        >
          <Table.Thead>
            <Table.Tr>
              <Table.Th>Name</Table.Th>
              <Table.Th>Protocol</Table.Th>
              <Table.Th>Subscribes to</Table.Th>
              <Table.Th>State</Table.Th>
              <Table.Th ta="right">Stored</Table.Th>
              <Table.Th ta="right">Received</Table.Th>
              <Table.Th>Last message</Table.Th>
              {showSpace && <Table.Th>Space</Table.Th>}
              <Table.Th w={96}>
                <span className="sr-only">Actions</span>
              </Table.Th>
            </Table.Tr>
          </Table.Thead>
          <Table.Tbody>
            {list.map((s) => {
              const readOnly = !!s.fromConfig || !can('messages:write', s.space ?? null);
              return (
                <Table.Tr key={s.id} data-testid="source-row" data-disabled={!s.enabled || undefined}>
                  <Table.Td>
                    <Stack gap={2} miw={0}>
                      <Group gap={6} wrap="wrap">
                        <Text size="sm" fw={600} c={s.enabled ? undefined : 'dimmed'} className="break-all">
                          {s.name}
                        </Text>
                        {s.fromConfig && (
                          <Tooltip label="Defined in the server configuration; read-only" withArrow>
                            <span className="fm-chip" data-testid="source-from-config">
                              config
                            </span>
                          </Tooltip>
                        )}
                        {!s.enabled && <span className="fm-chip">off</span>}
                      </Group>
                      {s.description && (
                        <Text size="xs" c="dimmed" className="truncate">
                          {s.description}
                        </Text>
                      )}
                    </Stack>
                  </Table.Td>
                  <Table.Td data-label="Protocol">
                    <ProtocolBadge protocol={s.protocol} />
                  </Table.Td>
                  <Table.Td data-label="Subscribes to">
                    <div>
                      <span className="fm-path break-all">{sourceTargets(s)}</span>
                      {s.protocol === 'kafka' && s.startFrom === 'earliest' && (
                        <Text size="xs" c="dimmed">
                          from the earliest offset
                        </Text>
                      )}
                      {s.protocol === 'mqtt' && (
                        <Text size="xs" c="dimmed">
                          QoS {s.qos ?? 1}
                        </Text>
                      )}
                    </div>
                  </Table.Td>
                  <Table.Td data-label="State">
                    <StateCell s={s} />
                  </Table.Td>
                  <Table.Td data-label="Stored" ta="right" className="tabular" data-testid="source-stored">
                    <span>
                      {s.status.stored ?? 0}
                      <Text span size="xs" c="dimmed">
                        {' '}
                        / {s.maxMessages ?? maxMessages}
                      </Text>
                    </span>
                  </Table.Td>
                  <Table.Td data-label="Received" ta="right" className="tabular">
                    <div>
                      {s.status.received ?? 0}
                      {(s.status.dropped ?? 0) > 0 && (
                        <Text size="xs" c="orange" data-testid="source-dropped">
                          {s.status.dropped} dropped
                        </Text>
                      )}
                    </div>
                  </Table.Td>
                  <Table.Td data-label="Last message">
                    <Time at={s.status.lastMessageAt} />
                  </Table.Td>
                  {showSpace && (
                    <Table.Td data-label="Space">
                      <SpaceBadge space={s.space} />
                    </Table.Td>
                  )}
                  <Table.Td>
                    <Group gap={2} wrap="nowrap" justify="flex-end">
                      <Tooltip label={readOnly ? 'View' : 'Edit'}>
                        <ActionIcon variant="subtle" color="gray" aria-label={`${readOnly ? 'View' : 'Edit'} ${s.name}`} onClick={() => open(s)} data-testid="source-edit">
                          {readOnly ? <IconEye size={16} /> : <IconPencil size={16} />}
                        </ActionIcon>
                      </Tooltip>
                      {!readOnly && (
                        <Tooltip label="Delete">
                          <ActionIcon
                            variant="subtle"
                            color="red"
                            aria-label={`Delete ${s.name}`}
                            data-testid="source-delete"
                            onClick={() => {
                              const choice = { purge: false };
                              confirmAction({
                                title: `Delete the source ${s.name}?`,
                                message: (
                                  <Stack gap="sm">
                                    <Text size="sm">Its consumer stops within a few seconds.</Text>
                                    <PurgeChoice onChange={(v) => (choice.purge = v)} />
                                  </Stack>
                                ),
                                confirm: 'Delete',
                                onConfirm: () => del.mutate({ s, purge: choice.purge }),
                              });
                            }}
                          >
                            <IconTrash size={16} />
                          </ActionIcon>
                        </Tooltip>
                      )}
                    </Group>
                  </Table.Td>
                </Table.Tr>
              );
            })}
          </Table.Tbody>
        </DataTable>
      )}
      {editing && <SourceForm key={editing.draft.id ?? 'new'} editing={editing} maxMessages={maxMessages} onClose={() => setEditing(null)} />}
    </Stack>
  );
}
