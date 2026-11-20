import { describe, expect, it } from 'vitest';
import { boundSpaces, can, covers, missingText, permissionsOf, seesAllSpaces, type Permissions } from './permissions';
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
