import { createContext } from 'react';
import type { Info, Role } from '../api/types';

export type AuthStatus = 'checking' | 'anonymous' | 'authenticated';

export interface AuthState {
  status: AuthStatus;
  info: Info | null;
  role: Role;
  name: string;
  expired: boolean;
  login: (token: string) => Promise<void>;
  logout: (expired?: boolean) => void;
  refresh: () => Promise<void>;
}

export const AuthContext = createContext<AuthState | null>(null);

