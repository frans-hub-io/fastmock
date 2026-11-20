import { describe, expect, it } from 'vitest';
import { BUILTIN_ROLES, boundSpaces, can, canInAllSpaces, covers, isAllSpaces, missingText, permissionsOf, roleInSpaces, seesAllSpaces, spaceScoped, tokensOffered, type Permissions } from './permissions';
import { loginErrorMessage, ssoReturnPath, takeLoginError } from './sso';

const scoped: Permissions = {
  global: ['info:read', 'chaos:read'],
  spaces: { payments: ['stubs:read', 'stubs:write', 'journal:read'], billing: ['stubs:read'] },
};

describe('can', () => {
  it('matches exact names and wildcards on either side', () => {
    expect(covers('stubs:write', 'stubs:write')).toBe(true);
    expect(covers('stubs:*', 'stubs:purge')).toBe(true);
    expect(covers('*', 'tokens:write')).toBe(true);
    expect(covers('stubs:read', 'stubs:*')).toBe(true);
    expect(covers('stubs:*', 'journal:read')).toBe(false);
    expect(covers('stubs:read', 'stubs:write')).toBe(false);
    expect(can({ global: ['state:*'], spaces: {} }, 'state:write')).toBe(true);
    expect(can({ global: [], spaces: { payments: ['files:*'] } }, 'files:write', 'payments')).toBe(true);
    expect(can({ global: [], spaces: { payments: ['files:*'] } }, 'files:write', 'billing')).toBe(false);
  });

  it('checks a space, the global area or anywhere', () => {
    expect(can(scoped, 'stubs:write', 'payments')).toBe(true);
    expect(can(scoped, 'stubs:write', 'billing')).toBe(false);
    expect(can(scoped, 'stubs:write', null)).toBe(false);
    expect(can(scoped, 'stubs:write')).toBe(true);
    expect(can(scoped, 'tokens:read')).toBe(false);
    expect(can(scoped, 'chaos:read', 'payments')).toBe(true);
    expect(can(null, 'stubs:read')).toBe(false);
  });

  it('never grants a global-only permission through a space', () => {
    const odd: Permissions = { global: [], spaces: { payments: ['audit:read', 'stubs:purge'] } };
    expect(can(odd, 'audit:read', 'payments')).toBe(false);
    expect(can(odd, 'stubs:purge')).toBe(false);
  });

  it('falls back to the built-in role sets for servers without permissions', () => {
    const viewer = permissionsOf({ name: 'ci', role: 'viewer' });
    expect(can(viewer, 'stubs:read')).toBe(true);
    expect(can(viewer, 'stubs:write')).toBe(false);
    const editor = permissionsOf({ name: 'ci', role: 'editor' });
    expect(can(editor, 'stubs:write', null)).toBe(true);
    expect(can(editor, 'audit:read')).toBe(false);
    expect(can(permissionsOf({ name: 'ops', role: 'admin' }), 'system:reset')).toBe(true);
  });

  it('gives viewers mail:read and editors every mail permission, all space-scoped', () => {
    expect(BUILTIN_ROLES.viewer).toContain('mail:read');
    expect(BUILTIN_ROLES.viewer).not.toContain('mail:write');
    expect(BUILTIN_ROLES.editor).toEqual(expect.arrayContaining(['mail:read', 'mail:write', 'mail:delete']));
    for (const p of ['mail:read', 'mail:write', 'mail:delete']) expect(spaceScoped(p)).toBe(true);
    const editor = permissionsOf({ name: 'ci', role: 'editor' });
    expect(can(editor, 'mail:delete', 'billing')).toBe(true);
    expect(can(permissionsOf({ name: 'ci', role: 'viewer' }), 'mail:write')).toBe(false);
    expect(can({ global: [], spaces: { billing: ['mail:*'] } }, 'mail:delete', 'billing')).toBe(true);
    expect(roleInSpaces(BUILTIN_ROLES.editor)).toBe('editor');
    expect(roleInSpaces(BUILTIN_ROLES.editor.filter((p) => !p.startsWith('mail:')))).toBe('custom');
  });

  it('tells global principals from space-bound ones', () => {
    expect(seesAllSpaces(scoped)).toBe(false);
    expect(seesAllSpaces({ global: ['stubs:read', 'info:read'], spaces: {} })).toBe(true);
    expect(seesAllSpaces({ global: ['chaos:*'], spaces: {} })).toBe(false);
    expect(seesAllSpaces({ global: ['journal:*'], spaces: {} })).toBe(true);
    expect(boundSpaces(scoped)).toEqual(['billing', 'payments']);
    expect(missingText('stubs:write', 'billing')).toBe('Requires the stubs:write permission in space billing');
    expect(missingText('audit:read')).toBe('Requires the audit:read permission');
  });
});

describe('SSO helpers', () => {
  it('builds return paths under /ui/ and reads login errors', () => {
    expect(ssoReturnPath('/stubs?tag=a')).toBe('/ui/stubs?tag=a');
    expect(ssoReturnPath('/')).toBe('/ui/');
    expect(ssoReturnPath('//evil.example.com')).toBe('/ui/');
    expect(takeLoginError('/?loginError=no_access&x=1')).toEqual({ code: 'no_access', rest: '/?x=1' });
    expect(takeLoginError('/stubs')).toEqual({ code: null, rest: '/stubs' });
    expect(loginErrorMessage('no_access')).toMatch(/no access/);
    expect(loginErrorMessage('weird')).toBe('Sign-in failed (weird).');
  });
});

describe('unknown roles and partial permissions', () => {
  it('grants an unknown role nothing instead of failing', () => {
    const p = permissionsOf({ name: 'x', role: 'auditor' as never });
    expect(p).toEqual({ global: [], spaces: {} });
    expect(seesAllSpaces(p)).toBe(false);
    expect(can(p, 'stubs:read')).toBe(false);
    expect(permissionsOf({ name: 'x', role: 'constructor' as never })).toEqual({ global: [], spaces: {} });
  });

  it('keeps what the server lists and drops malformed space entries', () => {
    const p = permissionsOf({ name: 'x', role: 'auditor' as never, permissions: { global: ['journal:read'], spaces: { billing: null as never, payments: ['stubs:read'] }, allSpaces: [] } });
    expect(p).toEqual({ global: ['journal:read'], spaces: { billing: [], payments: ['stubs:read'] } });
    expect(can(p, 'stubs:read', 'billing')).toBe(false);
    expect(can(p, 'stubs:read', 'payments')).toBe(true);
    expect(seesAllSpaces(p)).toBe(true);
  });
});

describe('all-spaces grants', () => {
  it('keeps allSpaces from the server and treats access:read as global-only', () => {
    const p = permissionsOf({
      name: 'ci',
      role: 'viewer',
      permissions: { global: ['info:read'], spaces: { billing: ['stubs:read', 'tokens:write'], payments: ['stubs:read', 'tokens:write'] }, allSpaces: ['stubs:read', 'tokens:write'] },
    });
    expect(p?.allSpaces).toEqual(['stubs:read', 'tokens:write']);
    expect(canInAllSpaces(p, 'tokens:write')).toBe(true);
    expect(canInAllSpaces(p, 'stubs:write')).toBe(false);
    expect(canInAllSpaces({ global: [], spaces: {}, allSpaces: ['*'] }, 'audit:read')).toBe(false);
    expect(canInAllSpaces({ global: ['tokens:*'], spaces: {} }, 'tokens:write')).toBe(true);
    expect(can({ global: [], spaces: { payments: ['access:read'] } }, 'access:read')).toBe(false);
    expect(can({ global: ['access:*'], spaces: {} }, 'access:read')).toBe(true);
    expect(seesAllSpaces({ global: ['access:*'], spaces: {} })).toBe(false);
    expect(isAllSpaces(['*'])).toBe(true);
    expect(isAllSpaces(['*', 'billing'])).toBe(false);
    expect(isAllSpaces([])).toBe(false);
  });
});

describe('tokensOffered', () => {
  it('needs the tokens feature or an explicit tokens permission, not a bare *', () => {
    expect(tokensOffered(['files'], { global: ['*'], spaces: {} })).toBe(false);
    expect(tokensOffered(undefined, { global: ['*'], spaces: {} })).toBe(false);
    expect(tokensOffered(['tokens'], { global: ['*'], spaces: {} })).toBe(true);
    expect(tokensOffered([], { global: [], spaces: { payments: ['tokens:write'] } })).toBe(true);
    expect(tokensOffered([], null)).toBe(false);
  });
});
