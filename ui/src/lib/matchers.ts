import type { StringMatcher } from '../api/types';

export const MATCHER_OPS = ['equalTo', 'contains', 'matches', 'doesNotMatch', 'present', 'absent'] as const;
export type MatcherOp = (typeof MATCHER_OPS)[number] | 'complex';

export const OP_LABELS: Record<MatcherOp, string> = {
  equalTo: 'equals',
  contains: 'contains',
  matches: 'matches regex',
  doesNotMatch: 'does not match',
  present: 'is present',
  absent: 'is absent',
  complex: 'advanced (JSON)',
};

export interface MatcherRow {
  key: string;
  op: MatcherOp;
  value: string;
  caseInsensitive: boolean;
  raw?: StringMatcher;
}

const SIMPLE_KEYS = new Set(['equalTo', 'contains', 'matches', 'doesNotMatch', 'absent', 'caseInsensitive']);

export function matcherToRow(key: string, m: StringMatcher): MatcherRow {
  const rec = m as Record<string, unknown>;
  const keys = Object.keys(m).filter((k) => rec[k] !== undefined && rec[k] !== null && !(k === 'caseInsensitive' && !rec[k]));
  const ci = m.caseInsensitive === true;
  if (keys.some((k) => !SIMPLE_KEYS.has(k))) return { key, op: 'complex', value: JSON.stringify(m), caseInsensitive: ci, raw: m };
  const valueKeys = keys.filter((k) => k !== 'caseInsensitive');
  if (valueKeys.length === 0) return { key, op: 'present', value: '', caseInsensitive: ci };
  if (valueKeys.length > 1) return { key, op: 'complex', value: JSON.stringify(m), caseInsensitive: ci, raw: m };
  const k = valueKeys[0];
  if (k === 'absent') return { key, op: m.absent ? 'absent' : 'present', value: '', caseInsensitive: ci };
  if (k === 'equalTo' || k === 'contains' || k === 'matches' || k === 'doesNotMatch') {
    return { key, op: k, value: m[k] ?? '', caseInsensitive: ci };
  }
  return { key, op: 'complex', value: JSON.stringify(m), caseInsensitive: ci, raw: m };
}

export function rowToMatcher(r: MatcherRow): StringMatcher {
  if (r.op === 'complex') return r.raw ?? {};
  const out: StringMatcher = {};
  if (r.op === 'absent') out.absent = true;
  else if (r.op !== 'present') out[r.op] = r.value;
  if (r.caseInsensitive && (r.op === 'equalTo' || r.op === 'contains')) out.caseInsensitive = true;
  return out;
}

export function mapToRows(m: Record<string, StringMatcher> | undefined): MatcherRow[] {
  return Object.entries(m ?? {}).map(([k, v]) => matcherToRow(k, v));
}

export function rowsToMap(rows: MatcherRow[]): Record<string, StringMatcher> | undefined {
  const out: Record<string, StringMatcher> = {};
  for (const r of rows) {
    if (!r.key.trim()) continue;
    out[r.key.trim()] = rowToMatcher(r);
  }
  return Object.keys(out).length ? out : undefined;
}
