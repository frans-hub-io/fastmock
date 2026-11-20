import { createContext } from 'react';
import type { components } from '../api/schema';

export type SpaceInfo = components['schemas']['SpaceInfo'];

export interface SpaceState {
  /** Spaces are configured on the server and the caller can see at least one. */
  enabled: boolean;
  spaces: SpaceInfo[];
  /** The selected space, or `null` for all spaces (or when spaces are not enabled). */
  current: string | null;
  /** "All spaces" is offered: the caller sees every space, or several of them. */
  allowAll: boolean;
  setCurrent: (space: string | null) => void;
}

export const NO_SPACES: SpaceState = { enabled: false, spaces: [], current: null, allowAll: true, setCurrent: () => undefined };

export const SpaceContext = createContext<SpaceState>(NO_SPACES);
