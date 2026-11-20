import { Alert, Anchor, Button, Center, Code, Divider, Paper, PasswordInput, Stack, Text, Title } from '@mantine/core';
import { IconKey, IconLogin2 } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Navigate, useLocation } from 'react-router';
import { errorMessage } from '../api/http';
import signatureDark from '../assets/brand/FastMock_by_FransHub_dark.svg';
import signatureLight from '../assets/brand/FastMock_by_FransHub_light.svg';
import { loginErrorMessage, takeLoginError } from '../auth/sso';
import { useAuth } from '../auth/useAuth';

interface AuthConfig {
  oidc: boolean;
  tokenLogin: boolean;
}

/** Sign-in methods; servers without the endpoint offer token sign-in only. */
async function fetchAuthConfig(): Promise<AuthConfig> {
  try {
    const res = await fetch('/__admin/auth/config', { headers: { Accept: 'application/json' }, credentials: 'omit', cache: 'no-store' });
    if (!res.ok) return { oidc: false, tokenLogin: true };
    const body = (await res.json()) as Partial<AuthConfig>;
    return { oidc: body.oidc === true, tokenLogin: body.tokenLogin !== false };
  } catch {
    return { oidc: false, tokenLogin: true };
  }
}

export function LoginPage() {
  const { status, login, loginSso, expired } = useAuth();
  const loc = useLocation();
  const [token, setToken] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [redirecting, setRedirecting] = useState(false);
  const config = useQuery({ queryKey: ['auth-config'], queryFn: fetchAuthConfig, staleTime: Infinity, retry: false });
  const rawFrom = (loc.state as { from?: string } | null)?.from ?? '/';
  const fromState = takeLoginError(rawFrom);
  const own = takeLoginError(`${loc.pathname}${loc.search}`);
  const loginError = own.code ?? fromState.code;
  const from = fromState.rest;
  const sso = config.data?.oidc === true;

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

  const startSso = () => {
    setRedirecting(true);
    loginSso(from.startsWith('/login') ? '/' : from);
  };

  return (
    <Center mih="100vh" p="md">
      <Paper withBorder shadow="sm" radius="lg" p="xl" w="100%" maw={440}>
        <form onSubmit={submit} noValidate>
          <Stack gap="md">
            <Stack gap={4} align="center">
              <Title order={1} size="h2" lh={1} mb="xs">
                <img src={signatureLight} alt="FastMock" width={240} height={57} className="fm-signature fm-on-light" />
                <img src={signatureDark} alt="FastMock" width={240} height={57} className="fm-signature fm-on-dark" />
              </Title>
              <Text c="dimmed" size="sm" ta="center">
                {sso ? 'Sign in with your organization account or an admin API token' : 'Sign in with an admin API token'}
              </Text>
            </Stack>
            {expired === 'session' && (
              <Alert color="yellow" role="alert" data-testid="session-expired">
                Your session has ended (it expired or was signed out elsewhere). Sign in again.
              </Alert>
            )}
            {expired === 'token' && (
              <Alert color="yellow" role="alert">
                Your session ended because the server rejected the token. Sign in again.
              </Alert>
            )}
            {loginError && (
              <Alert color="red" role="alert" title="SSO sign-in failed" data-testid="login-error">
                {loginErrorMessage(loginError)}
              </Alert>
            )}
            {sso && (
              <>
                <Button
                  size="md"
                  leftSection={<IconLogin2 size={18} />}
                  onClick={startSso}
                  loading={redirecting}
                  fullWidth
                  data-testid="sso-login"
                >
                  Sign in with SSO
                </Button>
                <Divider label="or use an admin token" labelPosition="center" />
              </>
            )}
            <PasswordInput
              label="Admin token"
              placeholder="fm_…"
              value={token}
              onChange={(e) => setToken(e.currentTarget.value)}
              autoComplete="off"
              autoFocus={!sso}
              required
              leftSection={<IconKey size={16} />}
              error={error}
              data-testid="token-input"
            />
            <Button type="submit" loading={busy} fullWidth variant={sso ? 'default' : 'filled'} data-testid="login-submit">
              {sso ? 'Sign in with token' : 'Sign in'}
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
