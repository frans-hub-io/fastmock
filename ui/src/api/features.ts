import { useQuery } from '@tanstack/react-query';
import type { Schema } from '../lib/jsonSchema';

export interface OpenApiDoc {
  openapi?: string;
  paths?: Record<string, Record<string, unknown>>;
  components?: { schemas?: Record<string, Schema> };
  [k: string]: unknown;
}

export async function fetchSpec(): Promise<OpenApiDoc | null> {
  try {
    const res = await fetch('/openapi.json', { headers: { Accept: 'application/json' }, credentials: 'omit' });
    if (!res.ok) return null;
    return (await res.json()) as OpenApiDoc;
  } catch {
    return null;
  }
}

export function useSpec() {
  return useQuery({ queryKey: ['openapi-spec'], queryFn: fetchSpec, staleTime: 5 * 60_000, retry: false });
}

function normalize(p: string): string {
  return p.replace(/\{[^}]+\}/g, '{}').replace(/\/+$/, '');
}

export function specHasPath(spec: OpenApiDoc | null | undefined, path: string, method?: string): boolean | undefined {
  if (spec === undefined) return undefined;
  if (spec === null || !spec.paths) return undefined;
  const want = normalize(path.startsWith('/__admin') || path === '/openapi.json' ? path : `/__admin${path}`);
  for (const [p, ops] of Object.entries(spec.paths)) {
    if (normalize(p) !== want) continue;
    if (!method) return true;
    return Object.keys(ops).some((m) => m.toLowerCase() === method.toLowerCase());
  }
  return false;
}

export type FeatureState = 'loading' | 'available' | 'missing' | 'unknown';

export function useFeature(path: string, method?: string): FeatureState {
  const spec = useSpec();
  if (spec.isLoading) return 'loading';
  const has = specHasPath(spec.data, path, method);
  if (has === undefined) return 'unknown';
  return has ? 'available' : 'missing';
}

export function useStubSchema(): { root: Schema; schema: Schema } | null {
  const spec = useSpec();
  const schemas = spec.data?.components?.schemas;
  const stub = schemas?.Stub;
  if (!spec.data || !stub) return null;
  return { root: spec.data, schema: stub };
}

export function useComponentSchema(name: string): { root: Schema; schema: Schema } | null {
  const spec = useSpec();
  const s = spec.data?.components?.schemas?.[name];
  if (!spec.data || !s) return null;
  return { root: spec.data, schema: s };
}
