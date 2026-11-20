import { Alert, Anchor, Button, Center, Group, Loader, Stack, Text, ThemeIcon } from '@mantine/core';
import { IconAlertTriangle, IconBook2, IconInbox, IconLock, IconPlugConnectedX } from '@tabler/icons-react';
import { useId, type ReactNode } from 'react';
import { ApiError, errorMessage, isForbidden, isNotFound } from '../api/http';
import { useFeature, useTokensApi } from '../api/features';
import classes from './States.module.css';

export function LoadingState({ label = 'Loading…' }: { label?: string }) {
  return (
    <Center py="xl" role="status" aria-live="polite">
      <Group gap="sm">
        <Loader size="sm" />
        <Text c="dimmed" size="sm">
          {label}
        </Text>
      </Group>
    </Center>
  );
}

/** The permission a 403 answer names in `required`, if any. */
function requiredPermission(error: unknown): string | undefined {
  if (!(error instanceof ApiError) || !error.body || typeof error.body !== 'object') return undefined;
  const r = (error.body as { required?: unknown }).required;
  return typeof r === 'string' && r ? r : undefined;
}

/** What a principal sees on a screen it may not use. */
export function NoAccessState({ perm, global }: { perm?: string; global?: boolean }) {
  return (
    <div data-testid="no-access">
      <EmptyState title="No access" icon={<IconLock size={26} />}>
        {perm
          ? `This screen requires the ${perm} permission${global ? ' for the whole server' : ''}.`
          : 'Your account may not use this screen.'}{' '}
        Ask an administrator to bind your user, group or token to a role that grants it.
      </EmptyState>
    </div>
  );
}

export function ErrorState({ error, onRetry }: { error: unknown; onRetry?: () => void }) {
  if (isNotFound(error)) return <NotAvailable />;
  if (isForbidden(error)) return <NoAccessState perm={requiredPermission(error)} />;
  return (
    <Alert color="red" icon={<IconAlertTriangle size={18} />} title="Request failed" role="alert">
      <Stack gap="xs">
        <Text size="sm">{errorMessage(error)}</Text>
        {onRetry && (
          <Group>
            <Button size="xs" variant="light" onClick={onRetry}>
              Retry
            </Button>
          </Group>
        )}
      </Stack>
    </Alert>
  );
}

/** A self-made illustration from the brand's shapes: two slanted cards (the monogram's italic), a gold dot. */
function Illustration({ icon }: { icon: ReactNode }) {
  const id = useId().replace(/:/g, '');
  return (
    <div className={classes.art} aria-hidden>
      <svg viewBox="0 0 160 120" width="160" height="120" focusable="false">
        <defs>
          <linearGradient id={`${id}f`} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0" className={classes.fransA} />
            <stop offset="1" className={classes.fransB} />
          </linearGradient>
          <linearGradient id={`${id}h`} x1="0" y1="0" x2="1" y2="1">
            <stop offset="0" className={classes.hubA} />
            <stop offset="1" className={classes.hubB} />
          </linearGradient>
        </defs>
        <ellipse cx="80" cy="66" rx="72" ry="46" className={classes.blob} />
        <g transform="skewX(-10) translate(14 0)">
          <rect x="36" y="22" width="70" height="56" rx="10" className={classes.back} />
          <rect x="50" y="36" width="74" height="58" rx="11" fill={`url(#${id}f)`} />
          <rect x="60" y="76" width="30" height="5" rx="2.5" className={classes.line} />
          <rect x="94" y="76" width="16" height="5" rx="2.5" className={classes.lineSoft} />
        </g>
        <circle cx="134" cy="28" r="9" fill={`url(#${id}h)`} />
        <circle cx="26" cy="92" r="5" className={classes.dot} />
        <circle cx="146" cy="88" r="3" className={classes.dot} />
      </svg>
      <span className={classes.artIcon}>{icon}</span>
    </div>
  );
}

/**
 * What a screen shows when it has nothing to list: an illustration, the title, one line of explanation, the
 * primary action and, optionally, where the documentation explains more.
 */
export function EmptyState({
  title,
  children,
  icon,
  action,
  docs,
  compact,
  testId,
}: {
  title: string;
  children?: ReactNode;
  icon?: ReactNode;
  action?: ReactNode;
  docs?: { label: string; href: string };
  compact?: boolean;
  testId?: string;
}) {
  const glyph = icon ?? <IconInbox size={26} />;
  return (
    <Center py={compact ? 'md' : 40} data-testid={testId}>
      <Stack align="center" gap={compact ? 6 : 'xs'} maw={480}>
        {compact ? (
          <ThemeIcon variant="light" size={44} radius="xl" color="fhPurple">
            {glyph}
          </ThemeIcon>
        ) : (
          <Illustration icon={glyph} />
        )}
        <Text fw={700} size={compact ? 'sm' : 'lg'} ta="center">
          {title}
        </Text>
        {children && (
          <Text c="dimmed" size="sm" ta="center" component="div">
            {children}
          </Text>
        )}
        {action && (
          <Group gap="xs" justify="center" mt={compact ? 4 : 'xs'}>
            {action}
          </Group>
        )}
        {docs && (
          <Anchor href={docs.href} target="_blank" rel="noopener noreferrer" size="xs" mt={4}>
            <Group gap={4} wrap="nowrap">
              <IconBook2 size={14} aria-hidden />
              {docs.label}
            </Group>
          </Anchor>
        )}
      </Stack>
    </Center>
  );
}

export function NotAvailable({ what }: { what?: string }) {
  return (
    <EmptyState title="Not available on this server" icon={<IconPlugConnectedX size={26} />}>
      {what ? `${what} is` : 'This feature is'} not offered by the FastMock server you are connected to. Upgrade the
      server or enable the feature to use this screen.
    </EmptyState>
  );
}

export function FeatureGate({ path, method, what, children }: { path: string; method?: string; what: string; children: ReactNode }) {
  const state = useFeature(path, method);
  if (state === 'loading') return <LoadingState />;
  if (state === 'missing') return <NotAvailable what={what} />;
  return <>{children}</>;
}

/** Renders `children` only when the server offers the managed-token API. */
export function TokensGate({ children }: { children: ReactNode }) {
  const state = useTokensApi();
  if (state === 'loading') return <LoadingState />;
  if (state === 'missing') return <NotAvailable what="Token management" />;
  return <>{children}</>;
}
