import { Alert, Button, Center, Group, Loader, Stack, Text, ThemeIcon } from '@mantine/core';
import { IconAlertTriangle, IconInbox, IconLock, IconPlugConnectedX } from '@tabler/icons-react';
import type { ReactNode } from 'react';
import { ApiError, errorMessage, isForbidden, isNotFound } from '../api/http';
import { useFeature, useTokensApi } from '../api/features';

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

export function EmptyState({ title, children, icon }: { title: string; children?: ReactNode; icon?: ReactNode }) {
  return (
    <Center py={48}>
      <Stack align="center" gap="xs" maw={460}>
        <ThemeIcon variant="light" size={48} radius="xl" color="gray">
          {icon ?? <IconInbox size={26} />}
        </ThemeIcon>
        <Text fw={600}>{title}</Text>
        {children && (
          <Text c="dimmed" size="sm" ta="center" component="div">
            {children}
          </Text>
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
