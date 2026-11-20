import type { Principal, Role } from '../api/types';

/** `resource:action` names as the server reports them in `principal.permissions`. */
export type Permission = string;

export interface Permissions {
  global: Permission[];
  spaces: Record<string, Permission[]>;
}

/**
 * Where an action applies: `undefined` means anywhere (any space or globally), `null` the global area,
 * a string one space.
 */
export type Where = string | null | undefined;

/** Permissions a space binding cannot grant (they act on the whole server). */
const GLOBAL_ONLY = new Set([
  'stubs:purge',
  'recordings:read',
  'recordings:write',
  'chaos:read',
  'chaos:write',
  'grpc:read',
  'grpc:write',
  'info:read',
  'audit:read',
  'system:reset',
]);

const GLOBAL_RESOURCES = new Set(['recordings', 'chaos', 'grpc', 'info', 'audit', 'system']);

const VIEWER = [
  'stubs:read',
  'journal:read',
  'files:read',
  'recordings:read',
  'callbacks:read',
  'state:read',
  'chaos:read',
  'grpc:read',
  'contracts:read',
  'info:read',
];
const EDITOR = [
  ...VIEWER,
  'stubs:write',
  'imports:run',
  'journal:delete',
  'files:write',
  'recordings:write',
  'callbacks:delete',
  'state:write',
  'chaos:write',
  'grpc:write',
  'contracts:details',
  'contracts:write',
];

/** Permission sets of the built-in roles, for servers that report only a role. */
export const BUILTIN_ROLES: Record<Role, Permission[]> = { viewer: VIEWER, editor: EDITOR, admin: ['*'] };

/** True when `held` (which may be `*` or `resource:*`) covers `needed` (which may be `resource:*`: any action). */
export function covers(held: Permission, needed: Permission): boolean {
  if (held === '*' || held === needed) return true;
  const [hr, ha] = held.split(':');
  const [nr, na] = needed.split(':');
  if (hr !== nr) return false;
  return ha === '*' || na === '*';
}

export function holds(list: readonly Permission[] | undefined, needed: Permission): boolean {
  return !!list?.some((h) => covers(h, needed));
}

export function spaceScoped(p: Permission): boolean {
  return !GLOBAL_ONLY.has(p);
}

/** `allowed(permission, space)` of the server: granted globally, or in that space for a space-scoped permission. */
export function can(perms: Permissions | null | undefined, needed: Permission, where?: Where): boolean {
  if (!perms) return false;
  if (holds(perms.global, needed)) return true;
  if (!spaceScoped(needed) || where === null) return false;
  if (where === undefined) return Object.values(perms.spaces).some((l) => holds(l, needed));
  return holds(perms.spaces[where], needed);
}

/** The principal's permissions, derived from the role when the server predates permissions. */
export function permissionsOf(principal: Partial<Principal> | null | undefined): Permissions | null {
  if (!principal) return null;
  const p = principal.permissions as Partial<Permissions> | undefined;
  if (p && Array.isArray(p.global)) return { global: p.global, spaces: p.spaces ?? {} };
  return principal.role ? { global: BUILTIN_ROLES[principal.role], spaces: {} } : null;
}

/** Spaces in which the principal is bound (global grants not included). */
export function boundSpaces(perms: Permissions | null | undefined): string[] {
  return Object.keys(perms?.spaces ?? {}).sort();
}

/** True when the principal holds some space-scoped permission globally, i.e. sees every space. */
export function seesAllSpaces(perms: Permissions | null | undefined): boolean {
  return !!perms?.global.some((p) => {
    if (p === '*') return true;
    if (p.endsWith(':*')) return !GLOBAL_RESOURCES.has(p.slice(0, -2));
    return p !== 'info:read' && spaceScoped(p);
  });
}

export function missingText(perm: Permission, where?: Where): string {
  const place = typeof where === 'string' ? ` in space ${where}` : '';
  return `Requires the ${perm} permission${place}`;
}

const KIND_LABEL: Record<string, string> = { token: 'token', apiToken: 'managed token', user: 'user', anonymous: 'anonymous' };

export function kindLabel(kind: string | undefined): string {
  return kind ? (KIND_LABEL[kind] ?? kind) : 'token';
}
