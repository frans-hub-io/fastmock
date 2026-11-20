import { Alert, Badge, Button, Checkbox, Group, List, Paper, SegmentedControl, Stack, TagsInput, Text, TextInput, Title } from '@mantine/core';
import { IconCamera, IconPlayerRecord, IconPlayerStop } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link } from 'react-router';
import { useFeature } from '../api/features';
import { client, unwrap } from '../api/client';
import { useApiMutation } from '../api/mutations';
import type { SnapshotResult } from '../api/types';
import { useCan } from '../auth/useAuth';
import { Permitted } from '../components/Permitted';
import { PageHeader, Time } from '../components/common';
import { ErrorState, LoadingState, NotAvailable } from '../components/States';

function SnapshotPanel() {
  const feature = useFeature('/__admin/recordings/snapshot', 'post');
  const [tag, setTag] = useState('snapshot');
  const [since, setSince] = useState('');
  const [matched, setMatched] = useState('all');
  const [pathPattern, setPathPattern] = useState('');
  const [captureHeaders, setCaptureHeaders] = useState<string[]>([]);
  const [scenarios, setScenarios] = useState(false);
  const [persist, setPersist] = useState(true);
  const snap = useApiMutation<void, SnapshotResult>({
    fn: () =>
      unwrap(client.POST('/__admin/recordings/snapshot', { body: {
        tag,
        filters: {
          ...(since ? { since: new Date(since).toISOString() } : {}),
          ...(matched !== 'all' ? { matched: matched === 'matched' } : {}),
          ...(pathPattern ? { request: { urlPathPattern: pathPattern } } : {}),
        },
        captureHeaders,
        repeatsAsScenarios: scenarios,
        persist,
        outputFormat: 'IDS',
      } })),
    invalidate: [['stubs'], ['info']],
    success: (d) => (d.persisted ? `Created ${d.ids?.length ?? d.stubs?.length ?? 0} stubs` : 'Snapshot generated (not saved)'),
  });
  if (feature === 'missing') return <NotAvailable what="Snapshotting the journal" />;
  return (
    <Paper withBorder p="md" radius="md">
      <Title order={3} size="h5" mb={4}>
        Snapshot the journal
      </Title>
      <Text size="sm" c="dimmed" mb="sm">
        Turn journal entries (for example, traffic that went through a proxy stub) into stubs. Bodies come from the redacted journal, so secrets never end up in stubs.
      </Text>
      <Stack gap="sm">
        <Group gap="sm" wrap="wrap" align="flex-end">
          <TextInput label="Tag" value={tag} onChange={(e) => setTag(e.currentTarget.value)} required w={180} />
          <TextInput label="Path regex" placeholder="/api/.*" value={pathPattern} onChange={(e) => setPathPattern(e.currentTarget.value)} w={200} className="mono" />
          <TextInput label="Since" type="datetime-local" value={since} onChange={(e) => setSince(e.currentTarget.value)} w={220} />
          <SegmentedControl
            value={matched}
            onChange={setMatched}
            data={[
              { label: 'All', value: 'all' },
              { label: 'Matched', value: 'matched' },
              { label: 'Unmatched', value: 'unmatched' },
            ]}
            aria-label="Which entries"
          />
        </Group>
        <TagsInput label="Capture request headers" placeholder="accept, x-tenant" value={captureHeaders} onChange={setCaptureHeaders} maxTags={32} />
        <Group>
          <Checkbox label="Repeated requests with different responses become a scenario" checked={scenarios} onChange={(e) => setScenarios(e.currentTarget.checked)} />
          <Checkbox label="Save stubs" checked={persist} onChange={(e) => setPersist(e.currentTarget.checked)} />
        </Group>
        <Group>
          <Permitted perm="recordings:write">
            <Button leftSection={<IconCamera size={16} />} disabled={!tag} loading={snap.isPending} onClick={() => snap.mutate()}>
              Take snapshot
            </Button>
          </Permitted>
        </Group>
        {snap.data && (
          <Alert color="green" title={snap.data.persisted ? 'Snapshot saved' : 'Snapshot preview'}>
            <Text size="sm">
              {snap.data.ids?.length ?? snap.data.stubs?.length ?? 0} stubs.{' '}
              {snap.data.persisted && (
                <Link to={`/stubs?tag=${encodeURIComponent(tag)}`}>Show stubs tagged {tag}</Link>
              )}
            </Text>
            {!!snap.data.warnings.length && (
              <List size="xs" mt={4}>
                {snap.data.warnings.slice(0, 20).map((w, i) => (
                  <List.Item key={i}>{w}</List.Item>
                ))}
              </List>
            )}
          </Alert>
        )}
      </Stack>
    </Paper>
  );
}

export default function Recordings() {
  const canEdit = useCan()('recordings:write');
  const status = useQuery({ queryKey: ['recordings'], queryFn: () => unwrap(client.GET('/__admin/recordings')), refetchInterval: 3000 });
  const [target, setTarget] = useState('');
  const [tag, setTag] = useState('recorded');
  const [captureHeaders, setCaptureHeaders] = useState<string[]>([]);
  const [requestHeaders, setRequestHeaders] = useState<string[]>([]);
  const start = useApiMutation<void>({
    fn: () => unwrap(client.POST('/__admin/recordings/start', { body: { targetBaseUrl: target, tag, captureHeaders, requestHeaders } })),
    invalidate: [['recordings']],
    success: 'Recording started',
  });
  const stop = useApiMutation<void, { stopped: boolean; recordedStubs: number }>({
    fn: () => unwrap(client.POST('/__admin/recordings/stop')),
    invalidate: [['recordings'], ['stubs'], ['info']],
    success: (d) => `Recording stopped, ${d.recordedStubs} stubs recorded`,
  });

  if (status.isLoading) return <LoadingState />;
  if (status.isError) return <ErrorState error={status.error} onRetry={() => void status.refetch()} />;
  const rec = status.data?.recording;

  return (
    <Stack gap="md">
      <PageHeader title="Recordings" description="Proxy unmatched requests to a real service and save the answers as stubs. Outbound calls obey the egress policy." />
      <Paper withBorder p="md" radius="md">
        {rec ? (
          <Stack gap="xs">
            <Group gap="xs">
              <Badge color="red" variant="dot" size="lg">
                Recording
              </Badge>
              <Text className="mono">{rec.targetBaseUrl}</Text>
            </Group>
            <Text size="sm">
              Tag <b>{rec.tag}</b> · started by {rec.startedBy ?? '?'} <Time at={rec.startedAt} />
            </Text>
            {canEdit && (
              <Group>
                <Button color="red" leftSection={<IconPlayerStop size={16} />} onClick={() => stop.mutate()} loading={stop.isPending}>
                  Stop recording
                </Button>
              </Group>
            )}
          </Stack>
        ) : (
          <Stack gap="sm">
            <Text size="sm" c="dimmed">
              Not recording.
            </Text>
            {canEdit && (
              <>
                <Group gap="sm" wrap="wrap" align="flex-end">
                  <TextInput label="Target base URL" placeholder="https://api.example.com" value={target} onChange={(e) => setTarget(e.currentTarget.value)} style={{ flex: '1 1 300px' }} required />
                  <TextInput label="Tag" value={tag} onChange={(e) => setTag(e.currentTarget.value)} w={180} required />
                </Group>
                <TagsInput label="Response headers to keep" value={captureHeaders} onChange={setCaptureHeaders} placeholder="x-rate-limit" />
                <TagsInput label="Request headers to match on" value={requestHeaders} onChange={setRequestHeaders} placeholder="accept" />
                <Group>
                  <Button leftSection={<IconPlayerRecord size={16} />} disabled={!target || !tag} loading={start.isPending} onClick={() => start.mutate()}>
                    Start recording
                  </Button>
                </Group>
              </>
            )}
          </Stack>
        )}
      </Paper>
      <SnapshotPanel />
    </Stack>
  );
}
