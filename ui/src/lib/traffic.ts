import type { JournalEntry } from '../api/types';

export interface Bucket {
  /** Start of the bucket, epoch milliseconds. */
  start: number;
  matched: number;
  unmatched: number;
}

/** Splits `windowMs` before `now` into `count` equal buckets and counts the entries of each, oldest first. */
export function bucketize(entries: readonly Pick<JournalEntry, 'at' | 'matched'>[], now: number, windowMs: number, count: number): Bucket[] {
  const size = windowMs / count;
  const from = now - windowMs;
  const out: Bucket[] = Array.from({ length: count }, (_, i) => ({ start: from + i * size, matched: 0, unmatched: 0 }));
  for (const e of entries) {
    const t = new Date(e.at).getTime();
    if (!Number.isFinite(t) || t < from || t > now) continue;
    const b = out[Math.min(count - 1, Math.floor((t - from) / size))];
    if (!b) continue;
    if (e.matched) b.matched++;
    else b.unmatched++;
  }
  return out;
}

/** The value at quantile `q` (0..1) of `values`, nearest-rank; undefined for no values. */
export function quantile(values: readonly number[], q: number): number | undefined {
  if (!values.length) return undefined;
  const sorted = [...values].sort((a, b) => a - b);
  const rank = Math.min(sorted.length - 1, Math.max(0, Math.ceil(q * sorted.length) - 1));
  return sorted[rank];
}

export interface LatencySummary {
  p50?: number;
  p95?: number;
  p99?: number;
  max?: number;
  count: number;
}

export function latency(entries: readonly Pick<JournalEntry, 'durationMs'>[]): LatencySummary {
  const d = entries.map((e) => e.durationMs).filter((v) => Number.isFinite(v));
  return { p50: quantile(d, 0.5), p95: quantile(d, 0.95), p99: quantile(d, 0.99), max: d.length ? Math.max(...d) : undefined, count: d.length };
}

export type StatusClass = '2xx' | '3xx' | '4xx' | '5xx' | 'other';

export function statusClasses(entries: readonly Pick<JournalEntry, 'status'>[]): Record<StatusClass, number> {
  const out: Record<StatusClass, number> = { '2xx': 0, '3xx': 0, '4xx': 0, '5xx': 0, other: 0 };
  for (const e of entries) {
    const c = Math.floor(e.status / 100);
    if (c === 2) out['2xx']++;
    else if (c === 3) out['3xx']++;
    else if (c === 4) out['4xx']++;
    else if (c === 5) out['5xx']++;
    else out.other++;
  }
  return out;
}

export interface StubHits {
  stubId: string;
  hits: number;
  lastAt: string;
}

/** Matches per stub, most hit first; `lastAt` is the newest match. */
export function hitsByStub(entries: readonly Pick<JournalEntry, 'stubId' | 'at' | 'matched'>[]): StubHits[] {
  const map = new Map<string, StubHits>();
  for (const e of entries) {
    if (!e.matched || !e.stubId) continue;
    const cur = map.get(e.stubId);
    if (!cur) map.set(e.stubId, { stubId: e.stubId, hits: 1, lastAt: e.at });
    else {
      cur.hits++;
      if (new Date(e.at).getTime() > new Date(cur.lastAt).getTime()) cur.lastAt = e.at;
    }
  }
  return [...map.values()].sort((a, b) => b.hits - a.hits || b.lastAt.localeCompare(a.lastAt));
}

/** A compact number: 1,284 / 12.9K / 4.2M. */
export function compact(n: number): string {
  if (Math.abs(n) < 10_000) return n.toLocaleString('en-US');
  if (Math.abs(n) < 1_000_000) return `${(n / 1000).toFixed(n < 100_000 ? 1 : 0).replace(/\.0$/, '')}K`;
  return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, '')}M`;
}

export function formatMs(ms: number | undefined): string {
  if (ms === undefined) return '—';
  if (ms < 1000) return `${Math.round(ms)} ms`;
  return `${(ms / 1000).toFixed(ms < 10_000 ? 2 : 1)} s`;
}

/** Rounded, readable axis maximum and ticks for a count axis starting at zero. */
export function niceTicks(max: number, count = 3): number[] {
  if (max <= 0) return [0, 1];
  const raw = max / count;
  const mag = 10 ** Math.floor(Math.log10(raw));
  const step = [1, 2, 5, 10].map((m) => m * mag).find((s) => s >= raw) ?? 10 * mag;
  const step2 = Math.max(1, step);
  const top = Math.ceil(max / step2) * step2;
  const ticks: number[] = [];
  for (let v = 0; v <= top + 1e-9; v += step2) ticks.push(Math.round(v));
  return ticks;
}
