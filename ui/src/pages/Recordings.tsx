import { Alert, Anchor, Button, Checkbox, Group, List, SegmentedControl, Stack, TagsInput, Text, TextInput } from '@mantine/core';
import { IconCamera, IconCircleCheck, IconPlayerRecord, IconPlayerStop, IconPlugConnected } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Link } from 'react-router';
import { useFeature } from '../api/features';
import { client, unwrap } from '../api/client';
import { useApiMutation } from '../api/mutations';
import type { SnapshotResult } from '../api/types';
import { useCan } from '../auth/useAuth';
import { Permitted } from '../components/Permitted';
import { DetailList, PageHeader, SectionCard, Time } from '../components/common';
import { ErrorState, LoadingState, NotAvailable } from '../components/States';
import { docsUrl } from '../lib/docs';
import { isHttpUrl, tagError } from '../lib/screens';
import classes from './Recordings.module.css';

function regexError(pattern: string): string | undefined {
  if (!pattern) return undefined;
  try {
    new RegExp(pattern);
    return undefined;
  } catch {
    return 'Not a valid regular expression';
  }
}

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
      unwrap(
        client.POST('/__admin/recordings/snapshot', {
          body: {
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
          },
        }),
      ),
    invalidate: [['stubs'], ['info']],
    success: (d) => (d.persisted ? `Created ${d.ids?.length ?? d.stubs?.length ?? 0} stubs` : 'Snapshot generated (not saved)'),
  });
  if (feature === 'missing') return <NotAvailable what="Snapshotting the journal" />;
  const tErr = tagError(tag, true);
  const rErr = regexError(pathPattern);
  return (
    <SectionCard
      title="Snapshot the journal"
      description="Turn journal entries (for example, traffic that went through a proxy stub) into stubs. Bodies come from the redacted journal, so secrets never end up in stubs."
      icon={<IconCamera size={16} />}
    >
      <form
        onSubmit={(e) => {
          e.preventDefault();
          if (!tErr && !rErr) snap.mutate();
        }}
      >
        <Stack gap="sm">
          <div className="fm-form-grid">
            <TextInput label="Tag" value={tag} onChange={(e) => setTag(e.currentTarget.value)} required error={tErr} classNames={{ input: 'mono' }} />
            <TextInput label="Path regex" placeholder="/api/.*" value={pathPattern} onChange={(e) => setPathPattern(e.currentTarget.value)} error={rErr} classNames={{ input: 'mono' }} />
            <TextInput label="Since" type="datetime-local" value={since} onChange={(e) => setSince(e.currentTarget.value)} />
            <Stack gap={4} className="fm-span-2">
              <Text size="sm" fw={500} id="snapshot-which">
                Entries
              </Text>
              <SegmentedControl
                value={matched}
                onChange={setMatched}
                data={[
                  { label: 'All', value: 'all' },
                  { label: 'Matched', value: 'matched' },
                  { label: 'Unmatched', value: 'unmatched' },
                ]}
                aria-label="Which entries"
                fullWidth
              />
            </Stack>
          </div>
          <TagsInput label="Capture request headers" description="Become header matchers of the generated stubs" placeholder="accept, x-tenant" value={captureHeaders} onChange={setCaptureHeaders} maxTags={32} />
          <Group gap="lg">
            <Checkbox label="Repeated requests with different responses become a scenario" checked={scenarios} onChange={(e) => setScenarios(e.currentTarget.checked)} />
            <Checkbox label="Save stubs" checked={persist} onChange={(e) => setPersist(e.currentTarget.checked)} />
          </Group>
          <Group justify="flex-end">
            <Permitted perm="recordings:write">
              <Button type="submit" leftSection={<IconCamera size={16} />} disabled={!!tErr || !!rErr} loading={snap.isPending}>
                {persist ? 'Take snapshot' : 'Preview snapshot'}
              </Button>
            </Permitted>
          </Group>
          {snap.data && (
            <Alert color="green" icon={<IconCircleCheck size={18} />} title={snap.data.persisted ? 'Snapshot saved' : 'Snapshot preview'}>
              <Text size="sm">
                {snap.data.ids?.length ?? snap.data.stubs?.length ?? 0} stubs.{' '}
                {snap.data.persisted && (
                  <Anchor component={Link} to={`/stubs?tag=${encodeURIComponent(tag)}`} size="sm">
                    Show stubs tagged {tag}
                  </Anchor>
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
      </form>
    </SectionCard>
  );
}

const STEPS = [
  { title: 'Proxy', text: 'A request that matches no stub goes to the target service.' },
  { title: 'Record', text: 'Its answer returns to the client and is saved as a stub with your tag.' },
  { title: 'Replay', text: 'The same request is then answered by the mock, even after you stop.' },
];

export default function Recordings() {
  const canEdit = useCan()('recordings:write');
  const status = useQuery({ queryKey: ['recordings'], queryFn: () => unwrap(client.GET('/__admin/recordings')), refetchInterval: 3000 });
  const [target, setTarget] = useState('');
  const [tag, setTag] = useState('recorded');
  const [captureHeaders, setCaptureHeaders] = useState<string[]>([]);
  const [requestHeaders, setRequestHeaders] = useState<string[]>([]);
  const [touched, setTouched] = useState(false);
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
  const urlError = target ? (isHttpUrl(target) ? undefined : 'An absolute http:// or https:// URL') : touched ? 'Where unmatched requests go' : undefined;
  const tErr = tagError(tag, touched);

  return (
    <Stack gap="md">
      <PageHeader
        title="Recordings"
        description="Proxy unmatched requests to a real service and save the answers as stubs. Outbound calls obey the egress policy."
        meta={
          <span className={classes.state} data-recording={rec ? true : undefined} data-testid="recording-state">
            <span className={classes.dot} aria-hidden />
            {rec ? 'Recording' : 'Not recording'}
          </span>
        }
      />
      {rec ? (
        <section className={classes.live} aria-label="Recording in progress">
          <Group justify="space-between" align="flex-start" wrap="wrap" gap="md">
            <Stack gap={6} miw={0}>
              <span className={classes.liveLabel}>
                <span className={classes.dot} aria-hidden /> Recording
              </span>
              <Text fw={700} size="lg" className="mono break-all">
                {rec.targetBaseUrl}
              </Text>
            </Stack>
            {canEdit && (
              <Button color="red" leftSection={<IconPlayerStop size={16} />} onClick={() => stop.mutate()} loading={stop.isPending}>
                Stop recording
              </Button>
            )}
          </Group>
          <DetailList
            columns={3}
            items={[
              { label: 'Tag', value: <span className="fm-chip fm-chip-soft">{rec.tag}</span> },
              { label: 'Started by', value: rec.startedBy ?? '?' },
              { label: 'Started', value: <Time at={rec.startedAt} /> },
            ]}
          />
        </section>
      ) : (
        <SectionCard title="Start recording" description="FastMock proxies unmatched requests to the target until you stop" icon={<IconPlayerRecord size={16} />}>
          <ol className={classes.steps}>
            {STEPS.map((s, i) => (
              <li key={s.title} className={classes.step}>
                <span className={classes.stepNo} aria-hidden>
                  {i + 1}
                </span>
                <span>
                  <strong>{s.title}</strong>
                  <Text size="xs" c="dimmed" component="span" display="block">
                    {s.text}
                  </Text>
                </span>
              </li>
            ))}
          </ol>
          {canEdit ? (
            <form
              onSubmit={(e) => {
                e.preventDefault();
                setTouched(true);
                if (target && tag && !urlError && !tErr) start.mutate();
              }}
            >
              <Stack gap="sm" mt="md">
                <div className="fm-form-grid">
                  <TextInput
                    label="Target base URL"
                    placeholder="https://api.example.com"
                    value={target}
                    onChange={(e) => setTarget(e.currentTarget.value)}
                    required
                    error={urlError}
                    classNames={{ input: 'mono' }}
                    className="fm-span-2"
                    leftSection={<IconPlugConnected size={16} />}
                  />
                  <TextInput label="Tag" value={tag} onChange={(e) => setTag(e.currentTarget.value)} required error={tErr} classNames={{ input: 'mono' }} />
                </div>
                <div className="fm-two-col">
                  <TagsInput label="Response headers to keep" description="content-type is always kept" value={captureHeaders} onChange={setCaptureHeaders} placeholder="x-rate-limit" />
                  <TagsInput label="Request headers to match on" description="Added as matchers of the recorded stubs" value={requestHeaders} onChange={setRequestHeaders} placeholder="accept" />
                </div>
                <Group justify="space-between" wrap="wrap" gap="sm">
                  <Anchor href={docsUrl('guide/recording')} target="_blank" rel="noopener noreferrer" size="xs">
                    Recording guide
                  </Anchor>
                  <Button type="submit" leftSection={<IconPlayerRecord size={16} />} loading={start.isPending}>
                    Start recording
                  </Button>
                </Group>
              </Stack>
            </form>
          ) : (
            <Text size="sm" c="dimmed" mt="sm">
              Not recording. An editor can start a recording here.
            </Text>
          )}
        </SectionCard>
      )}
      <SnapshotPanel />
    </Stack>
  );
}
