import { Button, Group, NumberInput, SegmentedControl, Stack, Switch, Text } from '@mantine/core';
import { IconArrowsShuffle, IconClockHour4, IconDeviceFloppy, IconPlugConnected } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { client, unwrap } from '../api/client';
import { useApiMutation } from '../api/mutations';
import type { components } from '../api/schema';
import type { Delay } from '../api/types';
import { useCan } from '../auth/useAuth';
import { PageHeader, SaveBar, SectionCard } from '../components/common';
import { ErrorState, FeatureGate, LoadingState } from '../components/States';
import { docsUrl } from '../lib/docs';
import { lognormalPercentile } from '../lib/screens';

type GlobalSettings = components['schemas']['GlobalSettings'];
type DelayMode = 'none' | 'fixed' | 'random' | 'lognormal';

function modeOf(d: Delay | null | undefined): DelayMode {
  if (!d) return 'none';
  if (d.lognormal) return 'lognormal';
  if (d.fixedMs != null) return 'fixed';
  if (d.minMs != null || d.maxMs != null) return 'random';
  return 'none';
}

const MODE_HELP: Record<DelayMode, string> = {
  none: 'Stubs answer after their own delay only.',
  fixed: 'Every response waits the same extra time.',
  random: 'Each response waits a random time between the two bounds, all values equally likely.',
  lognormal: 'Most responses wait about the median; a long tail waits much longer, like real networks.',
};

function SettingsInner() {
  const canEdit = useCan()('chaos:write');
  const q = useQuery({ queryKey: ['settings'], queryFn: () => unwrap(client.GET('/__admin/settings')) });
  const [defaultDelay, setDefaultDelay] = useState<number | ''>('');
  const [mode, setMode] = useState<DelayMode>('none');
  const [fixed, setFixed] = useState(0);
  const [min, setMin] = useState(0);
  const [max, setMax] = useState(0);
  const [median, setMedian] = useState(80);
  const [sigma, setSigma] = useState(0.4);
  const [pass, setPass] = useState(true);
  const [baseline, setBaseline] = useState('');

  const delay = (): Delay | null => {
    switch (mode) {
      case 'fixed':
        return { fixedMs: fixed };
      case 'random':
        return { minMs: min, maxMs: max };
      case 'lognormal':
        return { lognormal: { medianMs: median, sigma } };
      default:
        return null;
    }
  };
  const body = { fixedDelayMs: defaultDelay === '' ? null : defaultDelay, delay: delay(), proxyPassThrough: pass };
  const snapshot = JSON.stringify(body);

  const load = (s: GlobalSettings) => {
    const d = s.delay;
    const next = {
      defaultDelay: s.fixedDelayMs ?? ('' as const),
      mode: modeOf(d),
      fixed: d?.fixedMs ?? 0,
      min: d?.minMs ?? 0,
      max: d?.maxMs ?? 0,
      median: d?.lognormal?.medianMs ?? 80,
      sigma: d?.lognormal?.sigma ?? 0.4,
      pass: s.proxyPassThrough ?? true,
    };
    setDefaultDelay(next.defaultDelay);
    setMode(next.mode);
    setFixed(next.fixed);
    setMin(next.min);
    setMax(next.max);
    setMedian(next.median);
    setSigma(next.sigma);
    setPass(next.pass);
    const nd: Delay | null =
      next.mode === 'fixed' ? { fixedMs: next.fixed } : next.mode === 'random' ? { minMs: next.min, maxMs: next.max } : next.mode === 'lognormal' ? { lognormal: { medianMs: next.median, sigma: next.sigma } } : null;
    setBaseline(JSON.stringify({ fixedDelayMs: next.defaultDelay === '' ? null : next.defaultDelay, delay: nd, proxyPassThrough: next.pass }));
  };

  useEffect(() => {
    if (q.data) load(q.data);
  }, [q.data]);

  const save = useApiMutation<void, GlobalSettings>({
    fn: () => unwrap(client.PUT('/__admin/settings', { body })),
    invalidate: [['settings']],
    success: 'Settings saved; every replica applies them within 2 seconds',
    onSuccess: () => setBaseline(snapshot),
  });

  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const invalid = mode === 'random' && min > max;
  const dirty = baseline !== '' && snapshot !== baseline;

  return (
    <Stack gap="md">
      <PageHeader
        title="Settings"
        description="Server-wide response behaviour shared by every replica."
        meta={
          !canEdit && (
            <span className="fm-chip" data-testid="read-only-note">
              read-only
            </span>
          )
        }
      />
      <SectionCard title="Default delay" description="Used for stubs that have no delay of their own. Leave empty for none." icon={<IconClockHour4 size={16} />}>
        <NumberInput
          value={defaultDelay}
          onChange={(v) => setDefaultDelay(typeof v === 'number' ? v : '')}
          min={0}
          max={600000}
          suffix=" ms"
          placeholder="none"
          w={{ base: '100%', sm: 200 }}
          disabled={!canEdit}
          aria-label="Default delay in milliseconds"
        />
      </SectionCard>
      <SectionCard
        title="Added delay"
        description={
          <>
            Added to every stub response, on top of the stub&apos;s own delay. Capped by <span className="mono">FASTMOCK_MAX_DELAY</span>.
          </>
        }
        icon={<IconArrowsShuffle size={16} />}
      >
        <Stack gap="sm">
          <SegmentedControl
            value={mode}
            disabled={!canEdit}
            onChange={(v) => setMode(v)}
            data={[
              { label: 'None', value: 'none' },
              { label: 'Fixed', value: 'fixed' },
              { label: 'Uniform', value: 'random' },
              { label: 'Log-normal', value: 'lognormal' },
            ]}
            aria-label="Added delay type"
            w="fit-content"
            maw="100%"
          />
          <Text size="sm" c="dimmed">
            {MODE_HELP[mode]}
          </Text>
          {mode !== 'none' && (
            <Group gap="sm" align="flex-start" wrap="wrap">
              {mode === 'fixed' && <NumberInput label="Delay" suffix=" ms" value={fixed} onChange={(v) => setFixed(typeof v === 'number' ? v : 0)} min={0} max={600000} w={160} disabled={!canEdit} />}
              {mode === 'random' && (
                <>
                  <NumberInput label="Minimum" suffix=" ms" value={min} onChange={(v) => setMin(typeof v === 'number' ? v : 0)} min={0} max={600000} w={160} disabled={!canEdit} />
                  <NumberInput
                    label="Maximum"
                    suffix=" ms"
                    value={max}
                    onChange={(v) => setMax(typeof v === 'number' ? v : 0)}
                    min={0}
                    max={600000}
                    w={160}
                    disabled={!canEdit}
                    error={invalid ? 'The minimum must not exceed the maximum.' : undefined}
                  />
                </>
              )}
              {mode === 'lognormal' && (
                <>
                  <NumberInput label="Median" suffix=" ms" value={median} onChange={(v) => setMedian(typeof v === 'number' ? v : 0)} min={0} max={600000} w={160} disabled={!canEdit} />
                  <NumberInput label="Sigma" value={sigma} onChange={(v) => setSigma(typeof v === 'number' ? v : 0)} min={0} max={10} step={0.1} decimalScale={2} w={120} disabled={!canEdit} />
                  <Stack gap={4} mt={{ base: 0, sm: 24 }}>
                    <Group gap={6} wrap="wrap" aria-label="Expected delays">
                      <span className="fm-chip">p50 {lognormalPercentile(median, sigma, 50)} ms</span>
                      <span className="fm-chip">p95 {lognormalPercentile(median, sigma, 95)} ms</span>
                      <span className="fm-chip fm-chip-soft">p99 {lognormalPercentile(median, sigma, 99)} ms</span>
                    </Group>
                  </Stack>
                </>
              )}
            </Group>
          )}
        </Stack>
      </SectionCard>
      <SectionCard title="Proxying" description="Whether proxy stubs call their targets" icon={<IconPlugConnected size={16} />}>
        <Switch
          label="Proxy stubs forward requests"
          description="When off, stubs with a proxy response answer 404 instead of calling the target."
          checked={pass}
          onChange={(e) => setPass(e.currentTarget.checked)}
          disabled={!canEdit}
        />
      </SectionCard>
      <Text size="xs" c="dimmed">
        Delays per stub, distributions and caps are described in the{' '}
        <a href={docsUrl('guide/responses')} target="_blank" rel="noopener noreferrer">
          responses guide
        </a>
        .
      </Text>
      {canEdit && (
        <SaveBar dirty={dirty} testId="settings-save-bar">
          {dirty && q.data && (
            <Button variant="subtle" color="gray" onClick={() => load(q.data)}>
              Discard
            </Button>
          )}
          <Button leftSection={<IconDeviceFloppy size={16} />} onClick={() => save.mutate()} loading={save.isPending} disabled={invalid || !dirty} data-testid="save-settings">
            Save settings
          </Button>
        </SaveBar>
      )}
    </Stack>
  );
}

export default function Settings() {
  return (
    <FeatureGate path="/__admin/settings" method="get" what="Global settings">
      <SettingsInner />
    </FeatureGate>
  );
}
