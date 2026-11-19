import { Alert, Button, Group, NumberInput, Paper, SegmentedControl, Stack, Switch, Text, Title } from '@mantine/core';
import { useQuery } from '@tanstack/react-query';
import { useEffect, useState } from 'react';
import { client, unwrap } from '../api/client';
import { useApiMutation } from '../api/mutations';
import type { components } from '../api/schema';
import type { Delay } from '../api/types';
import { useCan } from '../auth/useAuth';
import { PageHeader } from '../components/common';
import { ErrorState, FeatureGate, LoadingState } from '../components/States';

type GlobalSettings = components['schemas']['GlobalSettings'];
type DelayMode = 'none' | 'fixed' | 'random' | 'lognormal';

function modeOf(d: Delay | null | undefined): DelayMode {
  if (!d) return 'none';
  if (d.lognormal) return 'lognormal';
  if (d.fixedMs != null) return 'fixed';
  if (d.minMs != null || d.maxMs != null) return 'random';
  return 'none';
}

function SettingsInner() {
  const canEdit = useCan('editor');
  const q = useQuery({ queryKey: ['settings'], queryFn: () => unwrap(client.GET('/__admin/settings')) });
  const [defaultDelay, setDefaultDelay] = useState<number | ''>('');
  const [mode, setMode] = useState<DelayMode>('none');
  const [fixed, setFixed] = useState(0);
  const [min, setMin] = useState(0);
  const [max, setMax] = useState(0);
  const [median, setMedian] = useState(80);
  const [sigma, setSigma] = useState(0.4);
  const [pass, setPass] = useState(true);

  useEffect(() => {
    const s = q.data;
    if (!s) return;
    setDefaultDelay(s.fixedDelayMs ?? '');
    const d = s.delay;
    setMode(modeOf(d));
    setFixed(d?.fixedMs ?? 0);
    setMin(d?.minMs ?? 0);
    setMax(d?.maxMs ?? 0);
    setMedian(d?.lognormal?.medianMs ?? 80);
    setSigma(d?.lognormal?.sigma ?? 0.4);
    setPass(s.proxyPassThrough ?? true);
  }, [q.data]);

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

  const save = useApiMutation<void, GlobalSettings>({
    fn: () =>
      unwrap(
        client.PUT('/__admin/settings', {
          body: { fixedDelayMs: defaultDelay === '' ? null : defaultDelay, delay: delay(), proxyPassThrough: pass },
        }),
      ),
    invalidate: [['settings']],
    success: 'Settings saved; every replica applies them within 2 seconds',
  });

  if (q.isLoading) return <LoadingState />;
  if (q.isError) return <ErrorState error={q.error} onRetry={() => void q.refetch()} />;
  const invalid = mode === 'random' && min > max;

  return (
    <Stack gap="md">
      <PageHeader title="Settings" description="Server-wide response behaviour shared by every replica." />
      <Paper withBorder p="md" radius="md">
        <Stack gap="lg">
          <div>
            <Title order={3} size="h5">
              Default delay
            </Title>
            <Text size="sm" c="dimmed" mb="xs">
              Used for stubs that have no delay of their own. Leave empty for none.
            </Text>
            <NumberInput
              value={defaultDelay}
              onChange={(v) => setDefaultDelay(typeof v === 'number' ? v : '')}
              min={0}
              max={600000}
              suffix=" ms"
              w={180}
              disabled={!canEdit}
              aria-label="Default delay in milliseconds"
            />
          </div>
          <div>
            <Title order={3} size="h5">
              Added delay
            </Title>
            <Text size="sm" c="dimmed" mb="xs">
              Added to every stub response, on top of the stub&apos;s own delay. Capped by FASTMOCK_MAX_DELAY.
            </Text>
            <Group gap="sm" align="flex-end" wrap="wrap">
              <SegmentedControl
                value={mode}
                disabled={!canEdit}
                onChange={setMode}
                data={[
                  { label: 'None', value: 'none' },
                  { label: 'Fixed', value: 'fixed' },
                  { label: 'Uniform', value: 'random' },
                  { label: 'Log-normal', value: 'lognormal' },
                ]}
                aria-label="Added delay type"
              />
              {mode === 'fixed' && <NumberInput label="ms" value={fixed} onChange={(v) => setFixed(typeof v === 'number' ? v : 0)} min={0} max={600000} w={130} disabled={!canEdit} />}
              {mode === 'random' && (
                <>
                  <NumberInput label="min ms" value={min} onChange={(v) => setMin(typeof v === 'number' ? v : 0)} min={0} max={600000} w={120} disabled={!canEdit} />
                  <NumberInput label="max ms" value={max} onChange={(v) => setMax(typeof v === 'number' ? v : 0)} min={0} max={600000} w={120} disabled={!canEdit} />
                </>
              )}
              {mode === 'lognormal' && (
                <>
                  <NumberInput label="median ms" value={median} onChange={(v) => setMedian(typeof v === 'number' ? v : 0)} min={0} max={600000} w={130} disabled={!canEdit} />
                  <NumberInput label="sigma" value={sigma} onChange={(v) => setSigma(typeof v === 'number' ? v : 0)} min={0} max={10} step={0.1} decimalScale={2} w={110} disabled={!canEdit} />
                </>
              )}
            </Group>
            {invalid && (
              <Alert color="red" mt="xs">
                The minimum must not exceed the maximum.
              </Alert>
            )}
          </div>
          <div>
            <Title order={3} size="h5" mb="xs">
              Proxying
            </Title>
            <Switch
              label="Proxy stubs forward requests"
              description="When off, stubs with a proxy response answer 404 instead of calling the target."
              checked={pass}
              onChange={(e) => setPass(e.currentTarget.checked)}
              disabled={!canEdit}
            />
          </div>
          {canEdit && (
            <Group>
              <Button onClick={() => save.mutate()} loading={save.isPending} disabled={invalid}>
                Save settings
              </Button>
            </Group>
          )}
        </Stack>
      </Paper>
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
