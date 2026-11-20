/**
 * A minimal OpenID Connect provider for the SSO end-to-end test: discovery, an authorization page with one button
 * per test user, the token endpoint (client_secret_basic, PKCE S256), ES256 ID tokens, JWKS and an end-session
 * page. Test only: no persistence, no consent, fixed users.
 *
 *   node e2e/fake-idp.ts <port>      issuer http://127.0.0.1:<port>/realms/test, client fastmock-ui / e2e-secret
 */
import { createHash, generateKeyPairSync, randomBytes, sign } from 'node:crypto';
import { createServer, type IncomingMessage, type ServerResponse } from 'node:http';

export const CLIENT_ID = 'fastmock-ui';
export const CLIENT_SECRET = 'e2e-secret';
export const USERS: Record<string, string[]> = { alice: ['payments-dev'], mallory: ['visitors'] };

interface Pending {
  user: string;
  nonce: string;
  challenge: string;
  redirect: string;
}

const b64url = (b: Buffer | string) => Buffer.from(b).toString('base64url');
const esc = (s: string) => s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

function body(req: IncomingMessage): Promise<URLSearchParams> {
  return new Promise((resolve, reject) => {
    const chunks: Buffer[] = [];
    req.on('data', (c: Buffer) => chunks.push(c));
    req.on('end', () => resolve(new URLSearchParams(Buffer.concat(chunks).toString('utf8'))));
    req.on('error', reject);
  });
}

export function startFakeIdp(port: number): Promise<() => Promise<void>> {
  const { privateKey, publicKey } = generateKeyPairSync('ec', { namedCurve: 'P-256' });
  const jwk = { ...publicKey.export({ format: 'jwk' }), kid: 'e2e', use: 'sig', alg: 'ES256' };
  const issuer = `http://127.0.0.1:${port}/realms/test`;
  const codes = new Map<string, Pending>();

  const jwt = (claims: Record<string, unknown>) => {
    const head = b64url(JSON.stringify({ alg: 'ES256', typ: 'JWT', kid: 'e2e' }));
    const payload = b64url(JSON.stringify(claims));
    const sig = sign('sha256', Buffer.from(`${head}.${payload}`), { key: privateKey, dsaEncoding: 'ieee-p1363' });
    return `${head}.${payload}.${b64url(sig)}`;
  };

  const send = (res: ServerResponse, status: number, type: string, text: string, headers: Record<string, string> = {}) => {
    res.writeHead(status, { 'content-type': type, 'cache-control': 'no-store', ...headers });
    res.end(text);
  };
  const json = (res: ServerResponse, status: number, v: unknown) => send(res, status, 'application/json', JSON.stringify(v));

  const server = createServer((req, res) => {
    void (async () => {
      const url = new URL(req.url ?? '/', issuer);
      const path = url.pathname.replace('/realms/test', '');
      if (path === '/.well-known/openid-configuration') {
        return json(res, 200, {
          issuer,
          authorization_endpoint: `${issuer}/authorize`,
          token_endpoint: `${issuer}/token`,
          userinfo_endpoint: `${issuer}/userinfo`,
          jwks_uri: `${issuer}/jwks`,
          end_session_endpoint: `${issuer}/logout`,
          code_challenge_methods_supported: ['S256'],
          token_endpoint_auth_methods_supported: ['client_secret_basic'],
          id_token_signing_alg_values_supported: ['ES256'],
        });
      }
      if (path === '/jwks') return json(res, 200, { keys: [jwk] });
      if (path === '/authorize' && req.method === 'GET') {
        const q = url.searchParams;
        if (q.get('client_id') !== CLIENT_ID || q.get('code_challenge_method') !== 'S256' || !q.get('state') || !q.get('nonce')) {
          return send(res, 400, 'text/plain', 'bad authorization request');
        }
        const hidden = ['redirect_uri', 'state', 'nonce', 'code_challenge']
          .map((k) => `<input type="hidden" name="${k}" value="${esc(q.get(k) ?? '')}">`)
          .join('');
        const buttons = Object.keys(USERS)
          .map((u) => `<button type="submit" name="user" value="${u}">Sign in as ${u}</button>`)
          .join(' ');
        return send(res, 200, 'text/html; charset=utf-8', `<!doctype html><title>Fake IdP</title><h1>Fake IdP</h1><form method="post" action="${issuer}/approve">${hidden}${buttons}</form>`);
      }
      if (path === '/approve' && req.method === 'POST') {
        const f = await body(req);
        const user = f.get('user') ?? '';
        if (!(user in USERS)) return send(res, 400, 'text/plain', 'unknown user');
        const code = b64url(randomBytes(24));
        const redirect = f.get('redirect_uri') ?? '';
        codes.set(code, { user, nonce: f.get('nonce') ?? '', challenge: f.get('code_challenge') ?? '', redirect });
        const loc = `${redirect}?code=${code}&state=${encodeURIComponent(f.get('state') ?? '')}`;
        return send(res, 302, 'text/plain', '', { location: loc });
      }
      if (path === '/token' && req.method === 'POST') {
        const basic = `Basic ${Buffer.from(`${CLIENT_ID}:${CLIENT_SECRET}`).toString('base64')}`;
        if (req.headers.authorization !== basic) return json(res, 401, { error: 'invalid_client' });
        const f = await body(req);
        const p = codes.get(f.get('code') ?? '');
        codes.delete(f.get('code') ?? '');
        const verifier = f.get('code_verifier') ?? '';
        if (!p || f.get('redirect_uri') !== p.redirect || b64url(createHash('sha256').update(verifier).digest()) !== p.challenge) {
          return json(res, 400, { error: 'invalid_grant' });
        }
        const now = Math.floor(Date.now() / 1000);
        const idToken = jwt({ iss: issuer, aud: CLIENT_ID, azp: CLIENT_ID, sub: `sub-${p.user}`, preferred_username: p.user, groups: USERS[p.user], nonce: p.nonce, iat: now, exp: now + 300 });
        return json(res, 200, { access_token: `at-${p.user}`, token_type: 'Bearer', expires_in: 300, id_token: idToken });
      }
      if (path === '/userinfo') return json(res, 200, { sub: 'unknown' });
      if (path === '/logout') return send(res, 200, 'text/html; charset=utf-8', '<!doctype html><title>Signed out</title><h1>Signed out of the fake IdP</h1>');
      return send(res, 404, 'text/plain', 'not found');
    })().catch((e: unknown) => send(res, 500, 'text/plain', String(e)));
  });

  return new Promise((resolve) => {
    server.listen(port, '127.0.0.1', () => {
      resolve(() => new Promise<void>((done) => server.close(() => done())));
    });
  });
}

const arg = process.argv[2];
if (arg && import.meta.url === `file://${process.argv[1] ?? ''}`) {
  void startFakeIdp(Number(arg)).then(() => console.log(`fake IdP on http://127.0.0.1:${arg}/realms/test`));
}
