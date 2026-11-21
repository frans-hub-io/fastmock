import { ActionIcon, Anchor, Button, Collapse, Group, Paper, ScrollArea, Stack, Text, TextInput, Tooltip, UnstyledButton } from '@mantine/core';
import {
  IconActivity,
  IconAlertTriangle,
  IconChartBar,
  IconChevronDown,
  IconClock,
  IconExternalLink,
  IconPlus,
  IconRefresh,
  IconRoute,
  IconSearch,
  IconServer,
  IconTerminal2,
  IconWebhook,
} from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useMemo, useState, type ReactNode } from 'react';
import { Link, useNavigate } from 'react-router';
import { client, unwrap } from '../api/client';
import { useVersion } from '../api/features';
import type { JournalEntry, Stub } from '../api/types';
import { useAuth, useCan, useSpaceQuery } from '../auth/useAuth';
import { BarList, Legend, SegmentBar, Sparkline, VolumeChart } from '../components/charts';
import { MethodBadge, PathText, SectionCard, StatusBadge, Time } from '../components/common';
import { EmptyState, ErrorState, LoadingState } from '../components/States';
import { formatDuration } from '../lib/format';
import { pathKindOf } from '../lib/path';
import { stubFromJournal, stubPath, stubTitle, urlKind } from '../lib/stubs';
import { bucketize, compact, formatMs, hitsByStub, latency, statusClasses } from '../lib/traffic';
import classes from './Dashboard.module.css';

const WINDOW_MIN = 60;
const BUCKETS = 30;
const BUCKET_MIN = WINDOW_MIN / BUCKETS;

function Stat({
  label,
  value,
  to,
  testId,
  sub,
  icon,
  trend,
}: {
  label: string;
  value: string | number | undefined;
  to?: string;
  testId?: string;
  sub?: ReactNode;
  icon: ReactNode;
  trend?: ReactNode;
}) {
  const content = (
    <Paper withBorder p="md" radius="md" className={classes.stat}>
      <Group justify="space-between" wrap="nowrap" gap="xs" align="flex-start">
        <Text size="sm" c="dimmed" fw={600}>
          {label}
        </Text>
        <span className={classes.statIcon} aria-hidden>
          {icon}
        </span>
      </Group>
      <Text className={classes.statValue} data-testid={testId}>
        {value ?? '—'}
      </Text>
      {sub && (
        <Text size="xs" c="dimmed" component="div" className={classes.statSub}>
          {sub}
        </Text>
      )}
      {trend && <div className={classes.trend}>{trend}</div>}
    </Paper>
  );
  return to ? (
    <Link to={to} className={classes.statLink}>
      {content}
    </Link>
  ) : (
    content
  );
}

function RawMetrics() {
  const [open, setOpen] = useState(false);
  const [filter, setFilter] = useState('fastmock_');
  const q = useQuery({
    queryKey: ['metrics'],
    queryFn: async () => {
      const res = await fetch('/metrics', { credentials: 'omit', cache: 'no-store' });
      if (!res.ok) throw new Error(`metrics: ${String(res.status)}`);
      return res.text();
    },
    enabled: open,
    refetchInterval: open ? 15_000 : false,
  });
  const lines = useMemo(() => {
    const f = filter.trim().toLowerCase();
    return (q.data ?? '')
      .split('\n')
      .filter((l) => l && !l.startsWith('#'))
      .filter((l) => !f || l.toLowerCase().includes(f))
      .slice(0, 400);
  }, [q.data, filter]);
  return (
    <Paper withBorder radius="md" className={classes.raw}>
      <UnstyledButton className={classes.rawHead} onClick={() => setOpen((o) => !o)} aria-expanded={open} data-testid="raw-metrics-toggle">
        <span className="fm-section-icon" aria-hidden>
          <IconTerminal2 size={16} />
        </span>
        <span className={classes.rawTitle}>
          <Text fw={700} size="sm">
            Raw metrics
          </Text>
          <Text size="xs" c="dimmed">
            The Prometheus text exposition at /metrics
          </Text>
        </span>
        <IconChevronDown size={18} className={classes.chevron} data-open={open || undefined} aria-hidden />
      </UnstyledButton>
      <Collapse expanded={open} keepMounted={false}>
        <div className={classes.rawBody}>
          <Group gap="xs" mb="xs" wrap="nowrap">
            <TextInput
              size="xs"
              className="fm-grow"
              leftSection={<IconSearch size={14} />}
              value={filter}
              onChange={(e) => setFilter(e.currentTarget.value)}
              aria-label="Filter metrics"
              placeholder="filter, e.g. fastmock_requests_total"
            />
            <Anchor href="/metrics" target="_blank" rel="noopener noreferrer" size="xs">
              <Group gap={4} wrap="nowrap">
                Open /metrics <IconExternalLink size={14} />
              </Group>
            </Anchor>
          </Group>
          {q.isLoading ? (
            <LoadingState />
          ) : q.isError ? (
            <ErrorState error={q.error} />
          ) : (
            <ScrollArea h={260}>
              <pre className="pre pre-flat">{lines.join('\n') || 'no matching series'}</pre>
            </ScrollArea>
          )}
        </div>
      </Collapse>
    </Paper>
  );
}

function NoRead({ perm }: { perm: string }) {
  return (
    <Text size="sm" c="dimmed">
      Requires the {perm} permission.
    </Text>
  );
}

export default function Dashboard() {
  const navigate = useNavigate();
  const { refresh, info: authInfo } = useAuth();
  const can = useCan();
  const spaceQ = useSpaceQuery();
  const readJournal = can('journal:read');
  const readCallbacks = can('callbacks:read');
  const readStubs = can('stubs:read');
  const canWrite = can('stubs:write');
  const info = useQuery({ queryKey: ['info'], queryFn: () => unwrap(client.GET('/__admin/info')), refetchInterval: 10_000 });
  const version = useVersion();
  const total = useQuery({
    queryKey: ['requests', 'count', 'all', spaceQ],
    queryFn: () => unwrap(client.GET('/__admin/requests/count', { params: { query: spaceQ } })),
    refetchInterval: 5_000,
    enabled: readJournal,
  });
  const unmatchedCount = useQuery({
    queryKey: ['requests', 'count', 'unmatched'],
    queryFn: () => unwrap(client.GET('/__admin/requests/count', { params: { query: { matched: false } } })),
    refetchInterval: 5_000,
    enabled: readJournal,
  });
  const callbacks = useQuery({
    queryKey: ['callbacks', 'dashboard', spaceQ],
    queryFn: () => unwrap(client.GET('/__admin/callbacks', { params: { query: { limit: 1000, ...spaceQ } } })),
    refetchInterval: 5_000,
    enabled: readCallbacks,
  });
  const recent = useQuery({
    queryKey: ['requests', 'recent-unmatched'],
    queryFn: () => unwrap(client.GET('/__admin/requests', { params: { query: { matched: false, limit: 6 } } })),
    refetchInterval: 5_000,
    enabled: readJournal,
  });
  const [now, setNow] = useState(() => Date.now());
  const win = useQuery({
    queryKey: ['requests', 'window', spaceQ],
    queryFn: async () => {
      const at = Date.now();
      const since = new Date(at - WINDOW_MIN * 60_000).toISOString();
      const r = await unwrap(client.GET('/__admin/requests', { params: { query: { since, limit: 2000, ...spaceQ } } }));
      setNow(at);
      return r;
    },
    refetchInterval: 10_000,
    enabled: readJournal,
  });
  const stubs = useQuery({
    queryKey: ['stubs', 'dashboard', spaceQ],
    queryFn: () => unwrap(client.GET('/__admin/stubs', { params: { query: { limit: 1000, ...spaceQ } } })),
    staleTime: 10_000,
    enabled: readStubs,
  });

  const entries = useMemo<JournalEntry[]>(() => win.data?.requests ?? [], [win.data]);
  const buckets = useMemo(() => bucketize(entries, now, WINDOW_MIN * 60_000, BUCKETS), [entries, now]);
  const lat = useMemo(() => latency(entries), [entries]);
  const classesCount = useMemo(() => statusClasses(entries), [entries]);
  const stubById = useMemo(() => new Map<string, Stub>((stubs.data?.stubs ?? []).map((s) => [s.id ?? '', s])), [stubs.data]);
  const top = useMemo(() => hitsByStub(entries).slice(0, 6), [entries]);

  if (info.isLoading) return <LoadingState />;
  if (info.isError) return <ErrorState error={info.error} onRetry={() => void info.refetch()} />;
  const i = info.data;
  const versionText = version.data?.version ?? i?.version;
  const extraVersion = version.data ? Object.entries(version.data).filter(([k, v]) => k !== 'version' && typeof v !== 'object') : [];
  const cbs = callbacks.data?.callbacks ?? [];
  const cbCount = (s: string) => cbs.filter((c) => c.status === s).length;
  const pendingCount = callbacks.data ? cbCount('pending') : undefined;
  const windowTotal = entries.length;
  const windowUnmatched = entries.filter((e) => !e.matched).length;
  const matchRate = windowTotal ? Math.round(((windowTotal - windowUnmatched) / windowTotal) * 1000) / 10 : undefined;
  const disabled = (stubs.data?.stubs ?? []).filter((s) => s.enabled === false).length;
  const capped = (win.data?.requests.length ?? 0) >= 2000;

  const live = entries.slice(0, 7);

  return (
    <Stack gap="lg">
      <section className={classes.hero} aria-labelledby="dash-title">
        <span className={classes.heroArt} aria-hidden />
        <div className={classes.heroMain}>
          <span className={classes.eyebrow}>
            <span className={classes.slash} aria-hidden />
            {i?.name ?? 'FastMock'} {versionText ? `v${versionText}` : ''} · {i?.storage ?? '?'} storage
          </span>
          <h2 id="dash-title" className={classes.heroTitle}>
            <span className={classes.nightGrad}>Dashboard</span>
          </h2>
          <p className={classes.lead}>
            {readJournal ? (
              windowTotal ? (
                <>
                  <strong>{compact(windowTotal)}{capped ? '+' : ''} requests</strong> reached the mock port in the last hour
                  {matchRate !== undefined && (
                    <>
                      ; <strong>{matchRate}%</strong> of them matched a stub
                    </>
                  )}
                  .
                </>
              ) : (
                <>No requests reached the mock port in the last hour. Send one and it shows up here within seconds.</>
              )
            ) : (
              <>Your stubs, traffic and callbacks at a glance.</>
            )}
          </p>
          <div className={classes.heroActions}>
            {canWrite && (
              <Button component={Link} to="/stubs/new" size="md" leftSection={<IconPlus size={18} />}>
                New stub
              </Button>
            )}
            <Button
              size="md"
              variant="outline"
              className={classes.ghost}
              leftSection={<IconRefresh size={18} />}
              onClick={() => void Promise.all([info.refetch(), win.refetch(), refresh()])}
            >
              Refresh
            </Button>
          </div>
          <ul className={classes.facts}>
            <li>up {formatDuration(i?.uptimeSeconds ?? 0)}</li>
            <li>
              signed in as {i?.principal.name ?? authInfo?.principal.name} ({i?.principal.role ?? authInfo?.principal.role})
            </li>
            {i?.authDisabled && <li>authentication disabled</li>}
          </ul>
        </div>
        {readJournal && (
          <figure className={classes.terminal} aria-label="Latest requests">
            <figcaption className={classes.termBar}>
              <span className={classes.dot} />
              <span className={classes.dot} />
              <span className={classes.dot} />
              <span className={classes.termTitle}>fastmock · latest requests</span>
              <span className={classes.termLive}>live</span>
            </figcaption>
            <div className={classes.termBody}>
              {live.length === 0 ? (
                <div className={classes.termMuted}># waiting for the first request…</div>
              ) : (
                live.map((r) => (
                  <div key={r.id} className={classes.termRow}>
                    <span className={classes.termMethod}>{r.method}</span>
                    <span className={classes.termPath}>
                      {r.path}
                      {r.query ? `?${r.query}` : ''}
                    </span>
                    <span className={classes.termStatus} data-tone={r.status >= 400 ? 'bad' : 'ok'}>
                      {r.status}
                    </span>
                    <span className={classes.termMs}>{formatMs(r.durationMs)}</span>
                  </div>
                ))
              )}
            </div>
          </figure>
        )}
      </section>

      {i?.warnings && i.warnings.length > 0 && (
        <Paper withBorder radius="md" p="sm" className={classes.warn}>
          <Group gap="xs" wrap="nowrap" align="flex-start">
            <IconAlertTriangle size={18} aria-hidden />
            <div>
              {i.warnings.map((w, k) => (
                <Text key={k} size="sm">
                  {w}
                </Text>
              ))}
            </div>
          </Group>
        </Paper>
      )}

      <div className={classes.bento}>
        <div className={`${classes.span7} ${classes.feature}`}>
          <SectionCard
            title="Traffic"
            description={`Requests per ${String(BUCKET_MIN)} minutes over the last hour${capped ? ' (latest 2,000)' : ''}`}
            icon={<IconChartBar size={16} />}
            actions={
              <Anchor component={Link} to="/requests" size="sm" fw={600} className="fm-nowrap">
                Open journal →
              </Anchor>
            }
          >
            {!readJournal ? (
              <NoRead perm="journal:read" />
            ) : win.isLoading ? (
              <LoadingState />
            ) : win.isError ? (
              <ErrorState error={win.error} />
            ) : (
              <Stack gap="sm">
                <Legend
                  items={[
                    { label: 'Matched', series: 1, value: compact(windowTotal - windowUnmatched) },
                    { label: 'Unmatched', series: 2, value: compact(windowUnmatched) },
                  ]}
                />
                <VolumeChart buckets={buckets} bucketMinutes={BUCKET_MIN} />
              </Stack>
            )}
          </SectionCard>
        </div>

        <div className={`${classes.span5} ${classes.stats}`}>
          <Stat
            label="Stubs loaded"
            value={i?.stubsLoaded}
            to="/stubs"
            testId="stat-stubs"
            icon={<IconRoute size={18} />}
            sub={stubs.data ? (disabled ? `${String(disabled)} disabled` : 'all enabled') : 'stubs you can read'}
          />
          <Stat
            label="Journal entries"
            value={readJournal ? (total.data ? compact(total.data.count) : undefined) : '—'}
            to="/requests"
            testId="stat-journal"
            icon={<IconActivity size={18} />}
            sub={readJournal ? `${compact(windowTotal)}${capped ? '+' : ''} in the last hour` : 'no journal access'}
            trend={readJournal && windowTotal > 0 ? <Sparkline values={buckets.map((b) => b.matched + b.unmatched)} /> : undefined}
          />
          <Stat
            label="Unmatched"
            value={readJournal ? (unmatchedCount.data ? compact(unmatchedCount.data.count) : undefined) : '—'}
            to="/requests?matched=false"
            testId="stat-unmatched"
            icon={<IconSearch size={18} />}
            sub={matchRate === undefined ? 'no traffic in the last hour' : `${String(matchRate)}% matched, last hour`}
            trend={readJournal && windowUnmatched > 0 ? <Sparkline values={buckets.map((b) => b.unmatched)} series={2} /> : undefined}
          />
          <Stat
            label="Pending callbacks"
            value={readCallbacks ? (pendingCount === undefined ? undefined : pendingCount >= 1000 ? '1000+' : pendingCount) : '—'}
            to="/callbacks?status=pending"
            testId="stat-callbacks"
            icon={<IconWebhook size={18} />}
            sub={readCallbacks ? (callbacks.data ? `${String(cbCount('failed'))} failed · ${String(cbCount('sent'))} sent` : undefined) : 'no callbacks access'}
          />
        </div>

        <div className={classes.span4}>
          <SectionCard title="Latency" description="Server time per request, last hour" icon={<IconClock size={16} />}>
            {!readJournal ? (
              <NoRead perm="journal:read" />
            ) : (
              <div className={classes.latency} data-testid="latency">
                {(
                  [
                    ['p50', lat.p50],
                    ['p95', lat.p95],
                    ['p99', lat.p99],
                    ['max', lat.max],
                  ] as const
                ).map(([k, v]) => (
                  <div key={k} className={classes.latCell}>
                    <span className="fm-chip">{k}</span>
                    <Text fw={800} className={classes.latValue}>
                      {formatMs(v)}
                    </Text>
                  </div>
                ))}
              </div>
            )}
          </SectionCard>
        </div>
        <div className={classes.span4}>
          <SectionCard title="Response status" description="Status classes, last hour" icon={<IconActivity size={16} />}>
            {!readJournal ? (
              <NoRead perm="journal:read" />
            ) : (
              <SegmentBar
                label="Responses by status class"
                testId="status-classes"
                parts={[
                  { key: '2xx', label: '2xx', value: classesCount['2xx'], tone: 'success' },
                  { key: '3xx', label: '3xx', value: classesCount['3xx'], tone: 'info' },
                  { key: '4xx', label: '4xx', value: classesCount['4xx'], tone: 'warning' },
                  { key: '5xx', label: '5xx', value: classesCount['5xx'], tone: 'danger' },
                ]}
              />
            )}
          </SectionCard>
        </div>
        <div className={classes.span4}>
          <SectionCard
            title="Callbacks"
            description="Webhook deliveries scheduled by stubs"
            icon={<IconWebhook size={16} />}
            actions={
              readCallbacks && (
                <Anchor component={Link} to="/callbacks" size="sm" fw={600} className="fm-nowrap">
                  Outbox →
                </Anchor>
              )
            }
          >
            {!readCallbacks ? (
              <NoRead perm="callbacks:read" />
            ) : callbacks.isLoading ? (
              <LoadingState />
            ) : callbacks.isError ? (
              <ErrorState error={callbacks.error} />
            ) : cbs.length === 0 ? (
              <EmptyState compact title="No callbacks yet" icon={<IconWebhook size={22} />}>
                Stubs with a callbacks section schedule deliveries here.
              </EmptyState>
            ) : (
              <SegmentBar
                label="Callbacks by status"
                testId="callback-status"
                parts={[
                  { key: 'sent', label: 'Sent', value: cbCount('sent'), tone: 'success' },
                  { key: 'pending', label: 'Pending', value: cbCount('pending'), tone: 'info' },
                  { key: 'failed', label: 'Failed', value: cbCount('failed'), tone: 'danger' },
                ]}
              />
            )}
          </SectionCard>
        </div>

        <div className={classes.span7}>
          <SectionCard
            title="Top stubs"
            description="Most matched stubs in the last hour"
            icon={<IconRoute size={16} />}
            actions={
              <Anchor component={Link} to="/stubs" size="sm" fw={600} className="fm-nowrap">
                All stubs →
              </Anchor>
            }
          >
            {!readJournal ? (
              <NoRead perm="journal:read" />
            ) : top.length === 0 ? (
              <EmptyState compact title="No matches yet" icon={<IconRoute size={22} />}>
                Stubs appear here once requests match them.
              </EmptyState>
            ) : (
              <BarList
                testId="top-stubs"
                rows={top.map((h) => {
                  const st = stubById.get(h.stubId);
                  return {
                    key: h.stubId,
                    value: h.hits,
                    label: (
                      <Group gap={8} wrap="nowrap" component="span">
                        <MethodBadge method={st?.request.method} />
                        <Anchor component={Link} to={`/stubs/${h.stubId}`} size="sm" fw={600} className="truncate">
                          {st ? stubTitle(st) : h.stubId.slice(0, 8)}
                        </Anchor>
                      </Group>
                    ),
                    hint: (
                      <Group gap={6} wrap="wrap" component="span">
                        {st && <PathText path={stubPath(st)} kind={pathKindOf(urlKind(st.request))} />}
                        <span>· last hit</span>
                        <Time at={h.lastAt} />
                      </Group>
                    ),
                  };
                })}
              />
            )}
          </SectionCard>
        </div>

        <div className={classes.span5}>
          <SectionCard
            title="Recent unmatched"
            description="Requests no stub answered"
            icon={<IconAlertTriangle size={16} />}
            actions={
              <Anchor component={Link} to="/near-misses" size="sm" fw={600} className="fm-nowrap">
                Near misses →
              </Anchor>
            }
          >
            {!readJournal ? (
              <NoRead perm="journal:read" />
            ) : recent.isLoading ? (
              <LoadingState />
            ) : recent.isError ? (
              <ErrorState error={recent.error} />
            ) : !recent.data?.requests.length ? (
              <EmptyState compact title="Every request matched a stub" icon={<IconActivity size={22} />}>
                Unmatched requests show up here as they arrive.
              </EmptyState>
            ) : (
              <ul className={classes.unmatched} data-testid="recent-unmatched">
                {recent.data.requests.map((r) => (
                  <li key={r.id} className={classes.unmatchedRow}>
                    <UnstyledButton className={classes.unmatchedMain} onClick={() => void navigate(`/requests?id=${String(r.id)}`)} aria-label={`${r.method} ${r.path}, open in the journal`}>
                      <MethodBadge method={r.method} />
                      <span className={classes.unmatchedPath}>
                        <PathText path={r.path} query={r.query} />
                        <span className={classes.unmatchedMeta}>
                          <StatusBadge status={r.status} />
                          <Time at={r.at} />
                        </span>
                      </span>
                    </UnstyledButton>
                    <Group gap={4} wrap="nowrap" className={classes.unmatchedActions}>
                      <Tooltip label="Explain why it did not match">
                        <ActionIcon variant="subtle" color="gray" component={Link} to={`/near-misses?id=${String(r.id)}`} aria-label={`Explain ${r.method} ${r.path}`}>
                          <IconSearch size={16} />
                        </ActionIcon>
                      </Tooltip>
                      {canWrite && (
                        <Tooltip label="Create a stub from this request">
                          <ActionIcon
                            variant="light"
                            onClick={() => void navigate('/stubs/new', { state: { stub: stubFromJournal(r) } })}
                            aria-label={`Create stub from ${r.method} ${r.path}`}
                            data-testid="dashboard-stub-from-request"
                          >
                            <IconPlus size={16} />
                          </ActionIcon>
                        </Tooltip>
                      )}
                    </Group>
                  </li>
                ))}
              </ul>
            )}
          </SectionCard>
        </div>

        <div className={classes.span12}>
          <SectionCard title="Server" description="This FastMock instance" icon={<IconServer size={16} />}>
            <dl className={classes.facts2}>
              <div className={classes.fact}>
                <dt>Version</dt>
                <dd>{versionText ?? '—'}</dd>
              </div>
              {extraVersion.map(([k, v]) => (
                <div key={k} className={classes.fact}>
                  <dt>{k}</dt>
                  <dd className="mono">{String(v)}</dd>
                </div>
              ))}
              <div className={classes.fact}>
                <dt>Storage</dt>
                <dd>{i?.storage}</dd>
              </div>
              <div className={classes.fact}>
                <dt>Uptime</dt>
                <dd>{formatDuration(i?.uptimeSeconds ?? 0)}</dd>
              </div>
              <div className={classes.fact}>
                <dt>Signed in</dt>
                <dd>
                  {i?.principal.name} <span className="fm-chip fm-chip-soft">{i?.principal.role ?? authInfo?.principal.role}</span>
                </dd>
              </div>
              <div className={classes.fact}>
                <dt>Plugins</dt>
                <dd>
                  {i?.plugins.length ? (
                    <Group gap={6}>
                      {i.plugins.map((p) => (
                        <Tooltip key={p.name} label={p.description} multiline maw={300}>
                          <span className="fm-chip" tabIndex={0}>
                            {p.name} {p.mount}
                            {p.admin ? ' · admin API' : ''}
                          </span>
                        </Tooltip>
                      ))}
                    </Group>
                  ) : (
                    <Text size="sm" c="dimmed" span>
                      none enabled
                    </Text>
                  )}
                </dd>
              </div>
            </dl>
          </SectionCard>
        </div>
      </div>

      <RawMetrics />
    </Stack>
  );
}
