import { useCallback, useContext, useMemo, useState } from 'react';
import type { Role } from '../api/types';
import { AuthContext, type AuthState } from './context';
import { can as allowed, missingText, type Permission, type Where } from './permissions';
import { SpaceContext, type SpaceState } from './spaceContext';

const RANK: Record<Role, number> = { viewer: 0, editor: 1, admin: 2 };

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside AuthProvider');
  return ctx;
}

export function hasRole(actual: Role, needed: Role): boolean {
  return RANK[actual] >= RANK[needed];
}

export function useSpace(): SpaceState {
  return useContext(SpaceContext);
}

/** `?space=` for collection requests: the selected space, or nothing for all spaces. */
export function useSpaceQuery(): { space?: string } {
  const { current } = useSpace();
  return current ? { space: current } : {};
}

export interface Can {
  /**
   * True when the principal holds the permission. Without `where`, in the selected space (or anywhere
   * when all spaces are selected); with `where`, in that space (`null`: the global area).
   */
  (perm: Permission, where?: Where): boolean;
  /** Tooltip text naming the missing permission, or `null` when it is held. */
  missing: (perm: Permission, where?: Where) => string | null;
}

export function useCan(): Can {
  const { permissions } = useAuth();
  const { current } = useSpace();
  const check = useCallback(
    (perm: Permission, where?: Where) => allowed(permissions, perm, where === undefined ? (current ?? undefined) : where),
    [permissions, current],
  );
  const missing = useCallback(
    (perm: Permission, where?: Where) => (check(perm, where) ? null : missingText(perm, where === undefined ? current : where)),
    [check, current],
  );
  return useMemo(() => Object.assign((perm: Permission, where?: Where) => check(perm, where), { missing }), [check, missing]);
}

/** Areas a form may write to with `perm`: `null` (the global area) and the spaces where `perm` is held. */
export function useSpaceOptions(perm: Permission): (string | null)[] {
  const { spaces } = useSpace();
  const can = useCan();
  return [...(can(perm, null) ? [null] : []), ...spaces.filter((s) => can(perm, s.name)).map((s) => s.name)];
}

/**
 * The area a create or import form writes to: the user's choice, else `initial`, else the selected space,
 * falling back to the first area where `perm` is held. Always `null` when the server has no spaces.
 */
export function useSpaceChoice(perm: Permission, initial?: string | null): [string | null, (space: string | null) => void] {
  const { enabled, current } = useSpace();
  const options = useSpaceOptions(perm);
  const [choice, setChoice] = useState<string | null | undefined>(initial);
  if (!enabled) return [null, setChoice];
  const preferred = choice !== undefined ? choice : current;
  const effective = options.includes(preferred) ? preferred : (options[0] ?? null);
  return [effective, setChoice];
}
