export function formatAbsolute(at: string | number | Date): string {
  const d = new Date(at);
  if (Number.isNaN(d.getTime())) return String(at);
  return d.toLocaleString(undefined, { hour12: false });
}

export function formatRelative(at: string | number | Date, now: number = Date.now()): string {
  const d = new Date(at).getTime();
  if (Number.isNaN(d)) return String(at);
  const diff = Math.round((now - d) / 1000);
  const abs = Math.abs(diff);
  const suffix = diff >= 0 ? 'ago' : 'from now';
  if (abs < 5) return 'just now';
  if (abs < 60) return `${abs}s ${suffix}`;
  if (abs < 3600) return `${Math.floor(abs / 60)}m ${suffix}`;
  if (abs < 86400) return `${Math.floor(abs / 3600)}h ${suffix}`;
  return `${Math.floor(abs / 86400)}d ${suffix}`;
}

export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '—';
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = Math.floor(seconds % 60);
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${s}s`;
  return `${s}s`;
}

export function formatBytes(n: number): string {
  if (!Number.isFinite(n)) return '—';
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
}

function prettyXml(xml: string): string {
  const tokens = xml.replace(/>\s+</g, '><').split(/(<[^>]+>)/).filter((t) => t.trim() !== '');
  let depth = 0;
  const lines: string[] = [];
  for (const t of tokens) {
    if (t.startsWith('</')) {
      depth = Math.max(0, depth - 1);
      lines.push('  '.repeat(depth) + t);
    } else if (t.startsWith('<') && !t.startsWith('<?') && !t.startsWith('<!') && !t.endsWith('/>')) {
      lines.push('  '.repeat(depth) + t);
      depth++;
    } else {
      lines.push('  '.repeat(depth) + t);
    }
  }
  const merged: string[] = [];
  for (let i = 0; i < lines.length; i++) {
    const cur = lines[i] ?? '';
    const next = lines[i + 1];
    const after = lines[i + 2];
    if (next !== undefined && after !== undefined && !next.trim().startsWith('<') && after.trim().startsWith('</') && cur.trim().startsWith('<') && !cur.trim().startsWith('</')) {
      merged.push(cur + next.trim() + after.trim());
      i += 2;
    } else {
      merged.push(cur);
    }
  }
  return merged.join('\n');
}

export function detectFormat(body: string, contentType?: string): 'json' | 'xml' | 'text' {
  const ct = (contentType ?? '').toLowerCase();
  const t = body.trimStart();
  if (ct.includes('json') || t.startsWith('{') || t.startsWith('[')) {
    try {
      JSON.parse(body);
      return 'json';
    } catch {
      /* not JSON */
    }
  }
  if (ct.includes('xml') || t.startsWith('<')) return 'xml';
  return 'text';
}

export function prettyBody(body: string, contentType?: string): string {
  if (body.length > 2_000_000) return body;
  const fmt = detectFormat(body, contentType);
  if (fmt === 'json') {
    try {
      return JSON.stringify(JSON.parse(body), null, 2);
    } catch {
      return body;
    }
  }
  if (fmt === 'xml') {
    try {
      return prettyXml(body);
    } catch {
      return body;
    }
  }
  return body;
}

export function pretty(v: unknown): string {
  return JSON.stringify(v, null, 2);
}

export function headerValue(headers: Record<string, string> | undefined | null, name: string): string | undefined {
  if (!headers) return undefined;
  const lower = name.toLowerCase();
  for (const [k, v] of Object.entries(headers)) if (k.toLowerCase() === lower) return v;
  return undefined;
}
