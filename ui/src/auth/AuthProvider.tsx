import { useQueryClient } from '@tanstack/react-query';
import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import { ApiError, errorFromResponse, rawFetch, tokenStore } from '../api/http';
import type { Info } from '../api/types';
import { AuthContext, type AuthState, type AuthStatus } from './context';

async function fetchInfo(token: string | null): Promise<Info> {
  const res = await rawFetch('GET', '/__admin/info', { token, skipAuthRedirect: true });
  if (!res.ok) throw await errorFromResponse(res);
  return (await res.json()) as Info;
}

export function AuthProvider({ children }: { children: ReactNode }) {
  const qc = useQueryClient();
  const [status, setStatus] = useState<AuthStatus>('checking');
  const [info, setInfo] = useState<Info | null>(null);
  const [expired, setExpired] = useState(false);

  const logout = useCallback(
    (wasExpired = false) => {
      tokenStore.set(null);
      qc.clear();
      setInfo(null);
      setExpired(wasExpired);
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
        if (!token && !i.authDisabled) {
          setStatus('anonymous');
          return;
        }
        setInfo(i);
        setStatus('authenticated');
      })
      .catch(() => {
        if (cancelled) return;
        if (token) tokenStore.set(null);
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
        setExpired(false);
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
      expired,
      login,
      logout,
      refresh,
    }),
    [status, info, expired, login, logout, refresh],
  );

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
}
