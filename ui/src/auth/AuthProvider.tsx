import { useQueryClient } from '@tanstack/react-query';
import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { ApiError, errorFromResponse, rawFetch, tokenStore, type AuthMode } from '../api/http';
import type { Info } from '../api/types';
import { AuthContext, type AuthState, type AuthStatus, type Expiry } from './context';
import { permissionsOf } from './permissions';
import { browser, ssoReturnPath } from './sso';

async function fetchInfo(token: string | null): Promise<Info> {
  const res = await rawFetch('GET', '/__admin/info', { token, skipAuthRedirect: true });
  if (!res.ok) throw await errorFromResponse(res);
  return (await res.json()) as Info;
}

/** The identity provider's end-session URL, if it is a plain http(s) URL. */
function endSessionUrl(raw: unknown): string | null {
  if (typeof raw !== 'string' || !raw) return null;
  try {
    const u = new URL(raw);
    return u.protocol === 'https:' || u.protocol === 'http:' ? u.toString() : null;
  } catch {
    return null;
  }
}

async function endSession(): Promise<void> {
  try {
    const res = await rawFetch('POST', '/__admin/auth/logout', { token: null, skipAuthRedirect: true });
    if (!res.ok) return;
    const body = (await res.json()) as { logoutUrl?: string | null };
    const url = endSessionUrl(body.logoutUrl);
    if (url) browser.assign(url);
  } catch {
    /* the local sign-out stands even when the server cannot be reached */
  }
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const qc = useQueryClient();
  const [status, setStatus] = useState<AuthStatus>('checking');
  const [info, setInfo] = useState<Info | null>(null);
  const [mode, setMode] = useState<AuthMode>(tokenStore.mode());
  const [expired, setExpired] = useState<Expiry>(null);

  const logout = useCallback(
    (wasExpired = false) => {
      const was = tokenStore.mode();
      if (was === 'session' && !wasExpired) void endSession();
      tokenStore.set(null);
      tokenStore.setSession(false);
      qc.clear();
      setInfo(null);
      setMode('none');
      setExpired(wasExpired ? (was === 'session' ? 'session' : 'token') : null);
      setStatus('anonymous');
    },
    [qc],
  );

  useEffect(() => {
    let cancelled = false;
    const token = tokenStore.get();
    fetchInfo(token)
      .then((i) => {
        if (cancelled) return;
        if (!token && !i.authDisabled) tokenStore.setSession(true);
        setMode(tokenStore.mode());
        setInfo(i);
        setStatus('authenticated');
      })
      .catch(() => {
        if (cancelled) return;
        if (token) tokenStore.set(null);
        setMode('none');
        setStatus('anonymous');
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(
    () =>
      tokenStore.onUnauthorized(() => {
        if (info?.authDisabled) return;
        logout(true);
      }),
    [logout, info],
  );

  const login = useCallback(
    async (token: string) => {
      const t = token.trim();
      if (!t) throw new ApiError(400, 'Paste a token first');
      try {
        const i = await fetchInfo(t);
        tokenStore.set(t);
        qc.clear();
        setInfo(i);
        setMode('token');
        setExpired(null);
        setStatus('authenticated');
      } catch (e) {
        if (e instanceof ApiError && (e.status === 401 || e.status === 403)) {
          throw new ApiError(e.status, 'The server rejected this token');
        }
        throw e;
      }
    },
    [qc],
  );

  const loginSso = useCallback((from: string) => {
    browser.assign(`/__admin/auth/login?return=${encodeURIComponent(ssoReturnPath(from))}`);
  }, []);

  const refresh = useCallback(async () => {
    const i = await fetchInfo(tokenStore.get());
    setInfo(i);
  }, []);

  const value = useMemo<AuthState>(
    () => ({
      status,
      info,
      role: info?.principal.role ?? 'viewer',
      name: info?.principal.name ?? '',
      kind: info?.principal.kind ?? 'token',
      mode,
      permissions: permissionsOf(info?.principal),
      expired,
      login,
      loginSso,
      logout,
      refresh,
    }),
    [status, info, mode, expired, login, loginSso, logout, refresh],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
