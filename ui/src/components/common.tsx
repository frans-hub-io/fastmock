import { ActionIcon, Anchor, Breadcrumbs, CopyButton, Group, Paper, Stack, Table, Text, Title, Tooltip } from '@mantine/core';
import { IconCheck, IconChevronRight, IconCopy, IconLayoutList, IconList } from '@tabler/icons-react';
import { Fragment, type ReactNode } from 'react';
import { Link, useLocation } from 'react-router';
import { formatAbsolute, formatRelative, prettyBody } from '../lib/format';
import { pathParts, type PathKind } from '../lib/path';
import { statusTone, type Density, type Tone } from '../lib/ui';
import classes from './common.module.css';
import { NAV, navGroupOf, navItemOf } from './nav';

export interface Crumb {
  label: string;
  to?: string;
}

/**
 * The top of every screen: breadcrumbs for nested screens, the title (the page's only level-2 heading), a short
 * description and the actions, primary last so it sits at the right edge.
 */
export function PageHeader({
  title,
  description,
  actions,
  breadcrumbs,
  icon,
  meta,
}: {
  title: string;
  description?: ReactNode;
  actions?: ReactNode;
  breadcrumbs?: Crumb[];
  icon?: ReactNode;
  meta?: ReactNode;
}) {
  const loc = useLocation();
  const item = loc.pathname === '/' ? NAV[0]?.items[0] : navItemOf(loc.pathname);
  const Glyph = item?.icon;
  const shownIcon = icon ?? (Glyph ? <Glyph size={22} stroke={1.6} /> : null);
  const kicker = breadcrumbs?.length ? undefined : navGroupOf(loc.pathname);
  return (
    <div className={classes.header}>
      {breadcrumbs && breadcrumbs.length > 0 && (
        <Breadcrumbs separator={<IconChevronRight size={12} aria-hidden />} separatorMargin={6} className={classes.crumbs} aria-label="Breadcrumb">
          {breadcrumbs.map((c, i) =>
            c.to ? (
              <Anchor key={i} component={Link} to={c.to} size="sm" c="dimmed" className={classes.crumb}>
                {c.label}
              </Anchor>
            ) : (
              <Text key={i} size="sm" c="dimmed" aria-current="page" className="truncate" maw={320}>
                {c.label}
              </Text>
            ),
          )}
        </Breadcrumbs>
      )}
      <div className={classes.row}>
        <Group gap="sm" wrap="nowrap" align="flex-start" className={classes.titleBlock}>
          {shownIcon && (
            <span className={classes.icon} aria-hidden>
              {shownIcon}
            </span>
          )}
          <Stack gap={4} miw={0}>
            {kicker && (
              <span className="fm-kicker" aria-hidden>
                {kicker}
              </span>
            )}
            <Title order={2} className={classes.title}>
              {title}
            </Title>
            {description && (
              <Text c="dimmed" size="sm" component="div" className={classes.description}>
                {description}
              </Text>
            )}
            {meta && <div className={classes.meta}>{meta}</div>}
          </Stack>
        </Group>
        {actions && (
          <Group gap="xs" wrap="wrap" className={classes.actions}>
            {actions}
          </Group>
        )}
      </div>
    </div>
  );
}

/** A titled card that groups one part of a screen. */
export function SectionCard({
  title,
  description,
  icon,
  actions,
  children,
  testId,
  padding = 'md',
}: {
  title: ReactNode;
  description?: ReactNode;
  icon?: ReactNode;
  actions?: ReactNode;
  children: ReactNode;
  testId?: string;
  padding?: string;
}) {
  return (
    <Paper withBorder radius="md" p={padding} data-testid={testId} className={classes.section}>
      <Group justify="space-between" align="flex-start" wrap="nowrap" gap="sm" mb="sm">
        <Group gap="sm" wrap="nowrap" align="center" miw={0}>
          {icon && (
            <span className="fm-section-icon" aria-hidden>
              {icon}
            </span>
          )}
          <div className={classes.sectionTitle}>
            <Title order={3} size="h5">
              {title}
            </Title>
            {description && (
              <Text size="xs" c="dimmed" component="div">
                {description}
              </Text>
            )}
          </div>
        </Group>
        {actions && (
          <Group gap={6} wrap="nowrap">
            {actions}
          </Group>
        )}
      </Group>
      {children}
    </Paper>
  );
}

const HTTP_METHODS = new Set(['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS']);

export function MethodBadge({ method }: { method?: string | null }) {
  const m = (method ?? 'ANY').toUpperCase();
  const known = HTTP_METHODS.has(m) || m === 'ANY';
  return (
    <span className={`fm-http ${classes.method}`} data-method={known ? m.toLowerCase() : undefined}>
      {m}
    </span>
  );
}

export function StatusBadge({ status }: { status: number }) {
  return (
    <span className="fm-status" data-tone={statusTone(status)}>
      {status}
    </span>
  );
}

/** A small status label with a coloured dot; the text always names the state. */
export function ToneBadge({ tone, children, testId }: { tone: Tone; children: ReactNode; testId?: string }) {
  return (
    <span className={`fm-status ${classes.toneText}`} data-tone={tone} data-testid={testId}>
      {children}
    </span>
  );
}

/** Text with a line-break opportunity before every slash, so long paths wrap between segments. */
function breakable(text: string): ReactNode {
  const segs = text.split('/');
  return segs.map((seg, i) => (
    <Fragment key={i}>
      {i > 0 && (
        <>
          <wbr />/
        </>
      )}
      {seg}
    </Fragment>
  ));
}

/** A request path in monospace, with template parameters (`{id}`) highlighted and the query dimmed. */
export function PathText({ path, query, kind, className }: { path: string; query?: string | null; kind?: PathKind; className?: string }) {
  const parts = pathParts(path, kind);
  return (
    <span className={`fm-path ${className ?? ''}`}>
      {kind && (kind === 'regex' || kind === 'any') && <span className="fm-path-kind">{kind === 'regex' ? 'regex' : 'any'}</span>}
      {parts.map((p, i) =>
        p.param ? (
          <span key={i} className="fm-path-param">
            {p.text}
          </span>
        ) : p.query ? (
          <span key={i} className="fm-path-query">
            {p.text}
          </span>
        ) : (
          <span key={i}>{kind === 'regex' ? p.text : breakable(p.text)}</span>
        ),
      )}
      {query ? <span className="fm-path-query">{`?${query}`}</span> : null}
    </span>
  );
}

export function Copy({ value, label = 'Copy' }: { value: string; label?: string }) {
  return (
    <CopyButton value={value} timeout={1500}>
      {({ copied, copy }) => (
        <Tooltip label={copied ? 'Copied' : label} withArrow>
          <ActionIcon variant="subtle" color={copied ? 'teal' : 'gray'} onClick={copy} aria-label={label}>
            {copied ? <IconCheck size={16} /> : <IconCopy size={16} />}
          </ActionIcon>
        </Tooltip>
      )}
    </CopyButton>
  );
}

export function Time({ at }: { at: string | null | undefined }) {
  if (!at) return <Text span c="dimmed">—</Text>;
  return (
    <Tooltip label={formatAbsolute(at)} withArrow>
      <Text span size="sm" className={classes.time} component="time" dateTime={at}>
        {formatRelative(at)}
      </Text>
    </Tooltip>
  );
}

export function DensityToggle({ value, onChange }: { value: Density; onChange: (d: Density) => void }) {
  const next = value === 'compact' ? 'comfortable' : 'compact';
  const label = next === 'compact' ? 'Compact rows' : 'Comfortable rows';
  return (
    <Tooltip label={label}>
      <ActionIcon variant="default" size="lg" onClick={() => onChange(next)} aria-label={label} visibleFrom="sm">
        {value === 'compact' ? <IconLayoutList size={18} /> : <IconList size={18} />}
      </ActionIcon>
    </Tooltip>
  );
}

/**
 * A data table in a card: the header stays visible while the rows scroll, rows follow the chosen density, and
 * with `stack` each row becomes a card on phones (label the cells with `data-label`).
 */
export function DataTable({
  children,
  testId,
  miw,
  density = 'comfortable',
  stack = true,
  footer,
}: {
  children: ReactNode;
  testId?: string;
  miw?: number;
  density?: Density;
  stack?: boolean;
  footer?: ReactNode;
}) {
  return (
    <Paper withBorder radius="md" className="fm-table-card">
      <div className="fm-table-wrap">
        <Table
          stickyHeader
          fz="sm"
          verticalSpacing={density === 'compact' ? 3 : 'xs'}
          horizontalSpacing="md"
          miw={miw}
          className={`fm-table ${stack ? 'stack-table' : ''}`}
          data-testid={testId}
        >
          {children}
        </Table>
      </div>
      {footer && <div className="fm-table-foot">{footer}</div>}
    </Paper>
  );
}

export function KeyValueTable({ data, empty = 'none' }: { data: Record<string, unknown> | null | undefined; empty?: string }) {
  const entries = Object.entries(data ?? {});
  if (!entries.length)
    return (
      <Text size="sm" c="dimmed">
        {empty}
      </Text>
    );
  return (
    <Table withTableBorder withColumnBorders fz="sm" verticalSpacing={4}>
      <Table.Tbody>
        {entries.map(([k, v]) => (
          <Table.Tr key={k}>
            <Table.Td w="35%" className="mono">
              {k}
            </Table.Td>
            <Table.Td className="mono break-all">{typeof v === 'string' ? v : JSON.stringify(v)}</Table.Td>
          </Table.Tr>
        ))}
      </Table.Tbody>
    </Table>
  );
}

export function BodyView({ body, contentType, empty = 'empty body' }: { body: string | null | undefined; contentType?: string | undefined; empty?: string }) {
  if (!body)
    return (
      <Text size="sm" c="dimmed">
        {empty}
      </Text>
    );
  const pretty = prettyBody(body, contentType);
  return (
    <div className={classes.body}>
      <pre className="pre">{pretty}</pre>
      <div className={classes.bodyCopy}>
        <Copy value={body} label="Copy body" />
      </div>
    </div>
  );
}
