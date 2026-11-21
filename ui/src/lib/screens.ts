import type { ChaosRule } from '../api/types';

const METHOD_ORDER = ['GET', 'POST', 'PUT', 'PATCH', 'DELETE', 'HEAD', 'OPTIONS'];

/** How many operations use each HTTP method, in the usual method order, then any other method by name. */
export function methodMix(operations: readonly { method?: string | null }[]): [string, number][] {
  const counts = new Map<string, number>();
  for (const o of operations) {
    if (!o.method) continue;
    const m = o.method.toUpperCase();
    counts.set(m, (counts.get(m) ?? 0) + 1);
  }
  const rank = (m: string) => {
    const i = METHOD_ORDER.indexOf(m);
    return i < 0 ? METHOD_ORDER.length : i;
  };
  return [...counts.entries()].sort((a, b) => rank(a[0]) - rank(b[0]) || a[0].localeCompare(b[0]));
}

export interface ChaosEffect {
  kind: 'status' | 'fault' | 'delay';
  text: string;
  status?: number;
}

/** What a chaos rule does to a matching request: the status it answers, the fault it injects, the latency it adds. */
export function chaosEffects(r: Pick<ChaosRule, 'status' | 'fault' | 'delay'>): ChaosEffect[] {
  const out: ChaosEffect[] = [];
  if (r.status) out.push({ kind: 'status', text: String(r.status), status: r.status });
  if (r.fault) out.push({ kind: 'fault', text: r.fault });
  const d = r.delay;
  if (d?.fixedMs) out.push({ kind: 'delay', text: `+${String(d.fixedMs)} ms` });
  if (d && (d.minMs != null || d.maxMs != null)) out.push({ kind: 'delay', text: `+${String(d.minMs ?? 0)}–${String(d.maxMs ?? 0)} ms` });
  if (d?.lognormal) out.push({ kind: 'delay', text: `~${String(d.lognormal.medianMs)} ms median` });
  return out;
}

export interface RuleIssues {
  id?: string;
  status?: string;
  effect?: string;
  limit?: string;
}

/** Why a new chaos rule cannot be added yet; empty when it can. */
export function chaosRuleIssues(draft: ChaosRule, existing: readonly ChaosRule[], max = 100): RuleIssues {
  const issues: RuleIssues = {};
  const id = draft.id.trim();
  if (!id) issues.id = 'Give the rule an id';
  else if (existing.some((r) => r.id === id)) issues.id = 'A rule with this id already exists';
  else if (id.length > 64) issues.id = 'At most 64 characters';
  if (draft.status != null && (draft.status < 100 || draft.status > 599)) issues.status = 'A status between 100 and 599';
  else if (draft.status != null && draft.fault) issues.status = 'Not used with a fault; clear it';
  if (!chaosEffects(draft).length) issues.effect = 'Choose a status, a fault or a delay, or the rule does nothing';
  if (existing.length >= max) issues.limit = `At most ${String(max)} rules`;
  return issues;
}

const Z = { 50: 0, 95: 1.6448536, 99: 2.3263479 } as const;

/** A percentile of a log-normal delay with the given median and sigma, rounded to whole milliseconds. */
export function lognormalPercentile(medianMs: number, sigma: number, p: 50 | 95 | 99): number {
  return Math.round(medianMs * Math.exp(Z[p] * sigma));
}

const TAG_RE = /^[A-Za-z0-9._:/-]{1,128}$/;

/** Why a tag is not accepted by the server (1 to 128 of letters, digits and `-_.:/`); undefined when it is. */
export function tagError(tag: string, required: boolean): string | undefined {
  if (!tag) return required ? 'Name a tag for the new stubs' : undefined;
  if (tag.length > 128) return 'At most 128 characters';
  if (!TAG_RE.test(tag)) return 'Letters, digits and - _ . : / only';
  return undefined;
}

/** Whether the text is an absolute http(s) URL with a host. */
export function isHttpUrl(text: string): boolean {
  try {
    const u = new URL(text.trim());
    return (u.protocol === 'http:' || u.protocol === 'https:') && u.hostname !== '';
  } catch {
    return false;
  }
}

/**
 * Why an upload folder would make file names the server refuses (it accepts `[A-Za-z0-9._-/]`, relative, without
 * empty, `.` or `..` segments); undefined when the folder is empty or fine. A trailing slash is allowed.
 */
export function folderError(prefix: string): string | undefined {
  const p = prefix.trim().replace(/\/+$/, '');
  if (!p) return undefined;
  if (!/^[A-Za-z0-9._/-]+$/.test(p)) return 'Letters, digits and - _ . / only';
  if (p.startsWith('/')) return 'Use a relative folder, without a leading slash';
  if (p.includes('//')) return 'Folder segments cannot be empty';
  if (p.includes('..') || p.split('/').some((s) => s === '.')) return 'No "." or ".." segments';
  return undefined;
}

/** A stored file name split into its folder (with the trailing slash) and its base name. */
export function splitName(name: string): { dir: string; base: string } {
  const i = name.lastIndexOf('/');
  return i < 0 ? { dir: '', base: name } : { dir: name.slice(0, i + 1), base: name.slice(i + 1) };
}

export type FileKind = 'json' | 'xml' | 'text' | 'image' | 'binary';

/** The broad kind of a stored body file, from its content type and, failing that, its extension. */
export function fileKind(name: string, contentType: string): FileKind {
  const ct = contentType.toLowerCase();
  const ext = name.toLowerCase().split('.').pop() ?? '';
  if (ct.includes('json') || ext === 'json') return 'json';
  if (ct.includes('xml') || ext === 'xml' || ext === 'wsdl') return 'xml';
  if (ct.startsWith('image/')) return 'image';
  if (ct.startsWith('text/') || ['txt', 'csv', 'html', 'md', 'yaml', 'yml'].includes(ext)) return 'text';
  return 'binary';
}

/** The size of a string in UTF-8 bytes. */
export function utf8Bytes(text: string): number {
  return new TextEncoder().encode(text).length;
}

/** How often an async mock publishes. */
export function frequencyLabel(ms: number | null | undefined): string {
  if (!ms) return 'on trigger';
  if (ms % 60_000 === 0) return `every ${String(ms / 60_000)} min`;
  if (ms % 1000 === 0) return `every ${String(ms / 1000)} s`;
  return `every ${String(ms)} ms`;
}

/**
 * Screens worth offering on a page that does not exist: those whose path or label shares a word with the
 * unknown path, best match first, then the given defaults.
 */
export function suggestScreens<T extends { to: string; label: string }>(pathname: string, items: readonly T[], fallback: readonly string[], max = 4): T[] {
  const words = pathname
    .toLowerCase()
    .split(/[^a-z0-9]+/)
    .filter((w) => w.length >= 3);
  const score = (i: T) => {
    const hay = `${i.to} ${i.label}`.toLowerCase();
    return words.reduce((s, w) => s + (hay.includes(w) ? 2 : hay.includes(w.slice(0, 4)) ? 1 : 0), 0);
  };
  const ranked = items
    .map((i) => ({ i, s: score(i) }))
    .filter((x) => x.s > 0)
    .sort((a, b) => b.s - a.s)
    .map((x) => x.i);
  const out: T[] = [];
  for (const i of [...ranked, ...fallback.map((to) => items.find((x) => x.to === to)).filter((x): x is T => !!x)]) {
    if (!out.includes(i)) out.push(i);
    if (out.length >= max) break;
  }
  return out;
}
