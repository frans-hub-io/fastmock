import { useContext } from 'react';
import type { Role } from '../api/types';
import { AuthContext, type AuthState } from './context';

const RANK: Record<Role, number> = { viewer: 0, editor: 1, admin: 2 };

export function useAuth(): AuthState {
  const ctx = useContext(AuthContext);
  if (!ctx) throw new Error('useAuth must be used inside AuthProvider');
  return ctx;
}

export function hasRole(actual: Role, needed: Role): boolean {
  return RANK[actual] >= RANK[needed];
}

export function useCan(needed: Role = 'editor'): boolean {
  const { role } = useAuth();
  return hasRole(role, needed);
}
