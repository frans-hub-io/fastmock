export type DiffOp = { kind: 'same' | 'add' | 'del'; text: string };

const MAX_CELLS = 4_000_000;

export function diffLines(a: string, b: string): DiffOp[] {
  const x = a.split('\n');
  const y = b.split('\n');
  const n = x.length;
  const m = y.length;
  if (n * m > MAX_CELLS) {
    return [...x.map((text) => ({ kind: 'del' as const, text })), ...y.map((text) => ({ kind: 'add' as const, text }))];
  }
  const w = m + 1;
  const lcs = new Uint32Array((n + 1) * (m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i * w + j] = x[i] === y[j] ? (lcs[(i + 1) * w + j + 1] ?? 0) + 1 : Math.max(lcs[(i + 1) * w + j] ?? 0, lcs[i * w + j + 1] ?? 0);
    }
  }
  const out: DiffOp[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    const xi = x[i] ?? '';
    const yj = y[j] ?? '';
    if (xi === yj) {
      out.push({ kind: 'same', text: xi });
      i++;
      j++;
    } else if ((lcs[(i + 1) * w + j] ?? 0) >= (lcs[i * w + j + 1] ?? 0)) {
      out.push({ kind: 'del', text: xi });
      i++;
    } else {
      out.push({ kind: 'add', text: yj });
      j++;
    }
  }
  while (i < n) out.push({ kind: 'del', text: x[i++] ?? '' });
  while (j < m) out.push({ kind: 'add', text: y[j++] ?? '' });
  return out;
}

export function sortKeysDeep(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(sortKeysDeep);
  if (v && typeof v === 'object') {
    return Object.fromEntries(
      Object.keys(v as Record<string, unknown>)
        .sort()
        .map((k) => [k, sortKeysDeep((v as Record<string, unknown>)[k])]),
    );
  }
  return v;
}

export function jsonDiff(a: unknown, b: unknown): DiffOp[] {
  const sa = a === undefined ? '' : JSON.stringify(sortKeysDeep(a), null, 2);
  const sb = b === undefined ? '' : JSON.stringify(sortKeysDeep(b), null, 2);
  return diffLines(sa, sb);
}
