/** Full-page navigation (the SSO redirects leave the single-page app); replaceable in tests. */
export const browser = {
  assign(url: string): void {
    window.location.assign(url);
  },
};

/** A return path under /ui/ for the SSO sign-in (the server accepts nothing else). */
export function ssoReturnPath(from: string): string {
  const path = from.startsWith('/') && !from.startsWith('//') ? from : '/';
  return `/ui${path}`;
}

const LOGIN_ERRORS: Record<string, string> = {
  invalid_state: 'The sign-in took too long or was started in another browser tab. Try again.',
  idp_error: 'The identity provider did not complete the sign-in.',
  failed: 'Sign-in failed: the answer of the identity provider could not be verified.',
  no_access: 'You signed in, but your account has no access to this FastMock server. Ask an administrator to bind your user or one of your groups to a role.',
  unavailable: 'Sign-in is unavailable right now (identity provider or session store unreachable). Try again later.',
};

/** Message for the `loginError` code the server puts on the redirect after a failed SSO sign-in. */
export function loginErrorMessage(code: string): string {
  return LOGIN_ERRORS[code] ?? `Sign-in failed (${code.slice(0, 40)}).`;
}

/** Splits `loginError` off a path with a query, returning the code and the path without it. */
export function takeLoginError(pathWithQuery: string): { code: string | null; rest: string } {
  const i = pathWithQuery.indexOf('?');
  if (i < 0) return { code: null, rest: pathWithQuery };
  const q = new URLSearchParams(pathWithQuery.slice(i + 1));
  const code = q.get('loginError');
  q.delete('loginError');
  const qs = q.toString();
  return { code, rest: pathWithQuery.slice(0, i) + (qs ? `?${qs}` : '') };
}
