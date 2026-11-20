import { useQuery } from '@tanstack/react-query';
import { useCallback, useMemo, useState, type ReactNode } from 'react';
import { client, unwrap } from '../api/client';
import { seesAllSpaces } from './permissions';
import { NO_SPACES, SpaceContext, type SpaceState } from './spaceContext';
import { useAuth } from './useAuth';

const KEY = 'fastmock.space';
const ALL = '*';

function readStored(): string | null {
  try {
    return window.localStorage.getItem(KEY);
  } catch {
    return null;
  }
}

function store(v: string): void {
  try {
    window.localStorage.setItem(KEY, v);
  } catch {
    /* the choice then lasts for this page only */
  }
}

export function SpaceProvider({ children }: { children: ReactNode }) {
  const { status, permissions, name } = useAuth();
  const [stored, setStored] = useState<string | null>(readStored);
  const q = useQuery({
    queryKey: ['spaces', name],
    queryFn: () => unwrap(client.GET('/__admin/spaces')),
    enabled: status === 'authenticated',
    staleTime: 60_000,
    retry: false,
  });
  const setCurrent = useCallback((space: string | null) => {
    const v = space ?? ALL;
    store(v);
    setStored(v);
  }, []);

  const value = useMemo<SpaceState>(() => {
    const spaces = q.data?.spaces ?? [];
    if (status !== 'authenticated' || !spaces.length) return NO_SPACES;
    const allowAll = seesAllSpaces(permissions) || spaces.length > 1;
    let current: string | null;
    if (stored && stored !== ALL && spaces.some((s) => s.name === stored)) current = stored;
    else if (allowAll) current = null;
    else current = spaces[0]?.name ?? null;
    return { enabled: true, spaces, current, allowAll, setCurrent };
  }, [q.data, status, permissions, stored, setCurrent]);

  return <SpaceContext.Provider value={value}>{children}</SpaceContext.Provider>;
}
