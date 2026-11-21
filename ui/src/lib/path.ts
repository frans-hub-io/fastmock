export type PathKind = 'exact' | 'template' | 'regex' | 'any';

export interface PathPart {
  text: string;
  param?: boolean;
  query?: boolean;
}

/** How a stub's URL matcher kind is shown. */
export function pathKindOf(urlKind: string): PathKind {
  if (urlKind === 'urlPathTemplate') return 'template';
  if (urlKind === 'urlPathPattern' || urlKind === 'urlPattern') return 'regex';
  if (urlKind === 'any') return 'any';
  return 'exact';
}

/**
 * Splits a path for display: `{name}` and `{name...}` template parameters become their own parts, and the query
 * of an exact URL (`/a?b=1`) is marked so it can be dimmed. Regular expressions are kept whole.
 */
export function pathParts(path: string, kind: PathKind = 'exact'): PathPart[] {
  if (kind === 'regex' || kind === 'any') return [{ text: path }];
  const out: PathPart[] = [];
  const q = path.indexOf('?');
  const head = q >= 0 ? path.slice(0, q) : path;
  const re = /\{[^}/]+\}/g;
  let last = 0;
  for (let m = re.exec(head); m; m = re.exec(head)) {
    if (m.index > last) out.push({ text: head.slice(last, m.index) });
    out.push({ text: m[0], param: true });
    last = m.index + m[0].length;
  }
  if (last < head.length) out.push({ text: head.slice(last) });
  if (q >= 0) out.push({ text: path.slice(q), query: true });
  return out;
}
