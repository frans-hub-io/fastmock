import { Alert, Anchor, Button, Code, Divider, Paper, PasswordInput, Stack, Text, Title } from '@mantine/core';
import { IconKey, IconLock, IconLogin2 } from '@tabler/icons-react';
import { useQuery } from '@tanstack/react-query';
import { useState } from 'react';
import { Navigate, useLocation } from 'react-router';
import { errorMessage } from '../api/http';
import monogramDark from '../assets/brand/FransHub_monogram_dark.svg';
import monogramLight from '../assets/brand/FransHub_monogram_light.svg';
import { fetchAuthConfig, loginErrorMessage, takeLoginError } from '../auth/sso';
import { useAuth } from '../auth/useAuth';
import classes from './Login.module.css';

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
    <div className={classes.page}>
      <div className={classes.backdrop} aria-hidden>
        <span className={classes.shapeA} />
        <span className={classes.shapeB} />
        <span className={classes.shapeC} />
      </div>
      <main className={classes.center}>
        <Paper withBorder radius="lg" p="xl" w="100%" maw={440} className={classes.card}>
          <form onSubmit={submit} noValidate>
            <Stack gap="lg">
              <Stack gap={6} align="center">
                <Title order={1} lh={1} className={classes.logo}>
                  <img src={monogramLight} alt="" width={63} height={36} className="fm-signature fm-on-light" />
                  <img src={monogramDark} alt="" width={63} height={36} className="fm-signature fm-on-dark" />
                  <span className="fm-wordmark fm-wordmark-lg">FastMock</span>
                </Title>
                <Text fw={800} size="xl" mt="xs" className="fm-login-title">
                  Sign in to the <span className="fm-grad">admin console</span>
                </Text>
                <Text c="dimmed" size="sm" ta="center">
                  {sso ? 'Use your organization account, or an admin API token' : 'Use an admin API token issued for this server'}
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
                    size="lg"
                    leftSection={<IconLogin2 size={20} />}
                    onClick={startSso}
                    loading={redirecting}
                    fullWidth
                    data-testid="sso-login"
                    className={classes.sso}
                  >
                    Sign in with SSO
                  </Button>
                  <Divider label="or use an admin token" labelPosition="center" />
                </>
              )}
              <Stack gap="sm">
                <PasswordInput
                  label="Admin token"
                  placeholder="fm_…"
                  value={token}
                  onChange={(e) => setToken(e.currentTarget.value)}
                  autoComplete="off"
                  autoFocus={!sso}
                  required
                  size={sso ? 'sm' : 'md'}
                  leftSection={<IconKey size={16} />}
                  error={error}
                  data-testid="token-input"
                  classNames={{ input: 'mono' }}
                />
                <Button type="submit" loading={busy} fullWidth size={sso ? 'sm' : 'md'} variant={sso ? 'default' : 'filled'} data-testid="login-submit">
                  {sso ? 'Sign in with token' : 'Sign in'}
                </Button>
              </Stack>
              <Text size="xs" c="dimmed" className={classes.help}>
                <IconLock size={14} aria-hidden className={classes.helpIcon} />
                <span>
                  The token stays in memory and in this tab&apos;s session storage only; it is sent as a bearer token to
                  <Code>/__admin</Code>. Create one with <Code>fastmock hash-token --generate</Code> and register its hash in
                  <Code>FASTMOCK_ADMIN_TOKENS</Code>. See the{' '}
                  <Anchor href="/openapi.json" target="_blank" rel="noopener noreferrer" size="xs">
                    API description
                  </Anchor>
                  .
                </span>
              </Text>
            </Stack>
          </form>
        </Paper>
        <Text size="xs" c="dimmed" mt="lg" ta="center">
          FastMock by FransHub
        </Text>
      </main>
    </div>
  );
}
