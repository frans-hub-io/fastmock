import { createContext } from 'react';
import type { Info, Role } from '../api/types';
import type { AuthMode } from '../api/http';
import type { Permissions } from './permissions';

export type AuthStatus = 'checking' | 'anonymous' | 'authenticated';

/** Why the user was sent back to the login page. */
export type Expiry = 'token' | 'session' | null;

export interface AuthState {
  status: AuthStatus;
  info: Info | null;
  role: Role;
  name: string;
  kind: string;
  mode: AuthMode;
  permissions: Permissions | null;
  expired: Expiry;
  login: (token: string) => Promise<void>;
  loginSso: (returnTo: string) => void;
  logout: (expired?: boolean) => void;
  refresh: () => Promise<void>;
}

export const AuthContext = createContext<AuthState | null>(null);
