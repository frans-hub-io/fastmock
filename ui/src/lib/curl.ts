export interface SampleRequest {
  method: string;
  path: string;
  query: string;
  headers: Record<string, string>;
  body: string;
}

export function shellQuote(s: string): string {
  if (s === '') return "''";
  if (/^[A-Za-z0-9_\-./:=@,+%]+$/.test(s)) return s;
  return `'${s.replace(/'/g, `'\\''`)}'`;
}

export function joinUrl(base: string, path: string, query: string): string {
  const b = base.replace(/\/+$/, '');
  const p = path.startsWith('/') ? path : `/${path}`;
  const q = query ? (query.startsWith('?') ? query : `?${query}`) : '';
  return `${b}${p}${q}`;
}

export function toCurl(base: string, req: SampleRequest): string {
  const parts = ['curl', '-i'];
  const method = req.method.toUpperCase();
  if (method !== 'GET' || req.body) parts.push('-X', method);
  for (const [k, v] of Object.entries(req.headers)) {
    if (!k.trim()) continue;
    parts.push('-H', shellQuote(`${k}: ${v}`));
  }
  if (req.body) parts.push('--data-raw', shellQuote(req.body));
  parts.push(shellQuote(joinUrl(base, req.path, req.query)));
  return parts.join(' ');
}

export function parseHeaderLines(text: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const line of text.split('\n')) {
    const i = line.indexOf(':');
    if (i <= 0) continue;
    const k = line.slice(0, i).trim();
    if (k) out[k] = line.slice(i + 1).trim();
  }
  return out;
}

export function headerLines(h: Record<string, string>): string {
  return Object.entries(h)
    .map(([k, v]) => `${k}: ${v}`)
    .join('\n');
}
