import { Alert, Anchor, Button, Center, Code, Paper, PasswordInput, Stack, Text, Title } from '@mantine/core';
import { IconKey, IconLock } from '@tabler/icons-react';
import { useState } from 'react';
import { Navigate, useLocation } from 'react-router';
import { errorMessage } from '../api/http';
import { useAuth } from '../auth/useAuth';

export function LoginPage() {
  const { status, login, expired } = useAuth();
  const loc = useLocation();
  const [token, setToken] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const from = (loc.state as { from?: string } | null)?.from ?? '/';

  if (status === 'authenticated') return <Navigate to={from.startsWith('/login') ? '/' : from} replace />;

  const submit = async (e: { preventDefault: () => void }) => {
    e.preventDefault();
    setBusy(true);
    setError(null);
    try {
      await login(token);
    } catch (err) {
      setError(errorMessage(err));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Center mih="100vh" p="md">
      <Paper withBorder shadow="sm" radius="lg" p="xl" w="100%" maw={440}>
        <form onSubmit={submit} noValidate>
          <Stack gap="md">
            <Stack gap={4} align="center">
              <IconLock size={34} stroke={1.5} aria-hidden />
              <Title order={1} size="h2">
                FastMock
              </Title>
              <Text c="dimmed" size="sm" ta="center">
                Sign in with an admin API token
              </Text>
            </Stack>
            {expired && (
              <Alert color="yellow" role="alert">
                Your session ended because the server rejected the token. Sign in again.
              </Alert>
            )}
            <PasswordInput
              label="Admin token"
              placeholder="fm_…"
              value={token}
              onChange={(e) => setToken(e.currentTarget.value)}
              autoComplete="off"
              autoFocus
              required
              leftSection={<IconKey size={16} />}
              error={error}
              data-testid="token-input"
            />
            <Button type="submit" loading={busy} fullWidth data-testid="login-submit">
              Sign in
            </Button>
            <Text size="xs" c="dimmed">
              The token stays in memory and in this tab&apos;s session storage only; it is sent as a bearer token to
              <Code>/__admin</Code>. Create one with <Code>fastmock hash-token --generate</Code> and register its hash in
              <Code>FASTMOCK_ADMIN_TOKENS</Code>. See the{' '}
              <Anchor href="/openapi.json" target="_blank" rel="noopener noreferrer" size="xs">
                API description
              </Anchor>
              .
            </Text>
          </Stack>
        </form>
      </Paper>
    </Center>
  );
}
