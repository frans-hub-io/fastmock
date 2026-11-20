import type { Principal, Role } from '../api/types';

/** `resource:action` names as the server reports them in `principal.permissions`. */
export type Permission = string;

export interface Permissions {
  global: Permission[];
  spaces: Record<string, Permission[]>;
  /** Permissions granted in every space through a `spaces: ["*"]` binding or token (absent when none). */
  allSpaces?: Permission[];
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
  'contracts:secrets',
  'info:read',
  'audit:read',
  'system:reset',
  'access:read',
]);

const GLOBAL_RESOURCES = new Set(['recordings', 'chaos', 'grpc', 'info', 'audit', 'system', 'access']);

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

/** True when a granted permission (possibly `*` or `resource:*`) has effect in a space, i.e. is not global-only. */
export function appliesInSpaces(p: Permission): boolean {
  return p === '*' || (p.endsWith(':*') ? !GLOBAL_RESOURCES.has(p.slice(0, -2)) : spaceScoped(p));
}

/** Effective permissions in one space: the global ones that apply in spaces plus the space's own grants. */
export function effectiveInSpace(perms: Permissions | null | undefined, space: string): Permission[] {
  if (!perms) return [];
  const fromGlobal = perms.global.filter(appliesInSpaces);
  return [...new Set([...fromGlobal, ...(perms.spaces[space] ?? [])])].sort();
}

/** The `spaces` entry of a binding or token that stands for every space, including spaces added later. */
export const ALL_SPACES = '*';

/** True when a binding's or token's space list is the all-spaces wildcard. */
export function isAllSpaces(spaces: readonly string[] | null | undefined): boolean {
  return !!spaces && spaces.length === 1 && spaces[0] === ALL_SPACES;
}

/**
 * True when the principal holds `needed` in every space, including spaces added later: globally, or through a
 * `spaces: ["*"]` grant for a space-scoped permission.
 */
export function canInAllSpaces(perms: Permissions | null | undefined, needed: Permission): boolean {
  if (!perms) return false;
  return holds(perms.global, needed) || (spaceScoped(needed) && holds(perms.allSpaces, needed));
}

/** The principal's permissions, derived from the role when the server predates permissions. */
export function permissionsOf(principal: Partial<Principal> | null | undefined): Permissions | null {
  if (!principal) return null;
  const p = principal.permissions as Partial<Permissions> | undefined;
  if (p && Array.isArray(p.global)) {
    const all = strings(p.allSpaces);
    return { global: strings(p.global), spaces: spaceLists(p.spaces), ...(all.length ? { allSpaces: all } : {}) };
  }
  if (!principal.role) return null;
  const builtin = Object.hasOwn(BUILTIN_ROLES, principal.role) ? BUILTIN_ROLES[principal.role] : undefined;
  return { global: builtin ?? [], spaces: {} };
}

function strings(list: unknown): Permission[] {
  return Array.isArray(list) ? list.filter((x): x is string => typeof x === 'string') : [];
}

function spaceLists(spaces: unknown): Record<string, Permission[]> {
  if (!spaces || typeof spaces !== 'object') return {};
  return Object.fromEntries(Object.entries(spaces).map(([k, v]) => [k, strings(v)]));
}

/**
 * True when the server offers the managed-token API: `/__admin/version` lists the `tokens` feature, or the
 * principal holds an explicit `tokens:` permission (a bare `*` does not count).
 */
export function tokensOffered(features: readonly string[] | undefined, perms: Permissions | null | undefined): boolean {
  if (features?.includes('tokens')) return true;
  if (!perms) return false;
  return [perms.global, ...Object.values(perms.spaces)].some((l) => l.some((x) => x.startsWith('tokens:')));
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

const VIEWER_IN_SPACES = VIEWER.filter(appliesInSpaces);
const EDITOR_IN_SPACES = EDITOR.filter(appliesInSpaces);
const ADMIN_IN_SPACES = [...EDITOR_IN_SPACES, 'tokens:read', 'tokens:write'];

/**
 * The built-in role (`admin`, `editor`, `viewer`) whose permissions in a space are exactly what `list` grants
 * there, or `custom` when no built-in role matches.
 */
export function roleInSpaces(list: readonly Permission[]): string {
  if (holds(list, '*')) return 'admin';
  const granted = list.filter(appliesInSpaces);
  const matches = (role: readonly Permission[]) => role.every((p) => holds(granted, p)) && granted.every((p) => holds(role, p));
  if (matches(ADMIN_IN_SPACES)) return 'admin';
  if (matches(EDITOR_IN_SPACES)) return 'editor';
  if (matches(VIEWER_IN_SPACES)) return 'viewer';
  return 'custom';
}

export interface AccessSummary {
  /** The role held everywhere; null when the principal holds no space-scoped permission globally. */
  global: string | null;
  /** The role held in every space through `spaces: ["*"]`, when that adds to the global permissions. */
  allSpaces: string | null;
  /** Spaces where the principal holds more than globally and in all spaces, with the role held there. */
  spaces: { space: string; role: string }[];
}

function adds(extra: readonly Permission[] | undefined, base: readonly Permission[]): boolean {
  return !!extra?.some((p) => !holds(base, p));
}

/**
 * What the principal may do, by scope, derived from its permissions. `role` is the server's deprecated
 * `principal.role`: the highest built-in role held everywhere, `viewer` also when the principal holds less.
 */
export function accessSummary(perms: Permissions | null | undefined, role: string): AccessSummary {
  if (!perms) return { global: role, allSpaces: null, spaces: [] };
  const viewerHeld = VIEWER.every((p) => holds(perms.global, p));
  const global = seesAllSpaces(perms) || !boundSpaces(perms).length ? (role !== 'viewer' || viewerHeld ? role : 'custom') : null;
  const base = perms.global.filter(appliesInSpaces);
  const all = perms.allSpaces ?? [];
  const allRole = adds(all, base) ? roleInSpaces([...base, ...all]) : null;
  const covered = [...base, ...all];
  const spaces = boundSpaces(perms)
    .filter((s) => adds(perms.spaces[s], covered))
    .map((s) => ({ space: s, role: roleInSpaces(effectiveInSpace(perms, s)) }));
  return { global, allSpaces: allRole, spaces };
}

/** One line for the header, e.g. `viewer · editor in all spaces`, `editor in payments` or `3 spaces`. */
export function summaryLabel(s: AccessSummary): string {
  const scoped = s.allSpaces
    ? `${s.allSpaces} in all spaces`
    : s.spaces.length === 1 && s.spaces[0]
      ? `${s.spaces[0].role} in ${s.spaces[0].space}`
      : s.spaces.length > 1
        ? `${s.spaces.length} spaces`
        : null;
  return [s.global, scoped].filter(Boolean).join(' · ') || 'no access';
}

export function missingText(perm: Permission, where?: Where): string {
  const place = typeof where === 'string' ? ` in space ${where}` : '';
  return `Requires the ${perm} permission${place}`;
}

const KIND_LABEL: Record<string, string> = { token: 'token', apiToken: 'managed token', user: 'user', anonymous: 'anonymous' };

export function kindLabel(kind: string | undefined): string {
  return kind ? (KIND_LABEL[kind] ?? kind) : 'token';
}
