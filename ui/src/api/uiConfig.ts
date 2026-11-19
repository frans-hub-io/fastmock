import { useQuery } from '@tanstack/react-query';

export interface UiConfig {
  dataPlanePort?: number;
  dataPlaneScheme?: string;
  dataPlaneUrl?: string;
}

const BASE_KEY = 'fastmock.dataPlaneBaseUrl';

export function useUiConfig() {
  return useQuery({
    queryKey: ['ui-config'],
    queryFn: async (): Promise<UiConfig> => {
      try {
        const res = await fetch('/ui/config.json', { credentials: 'omit' });
        if (!res.ok) return {};
        return (await res.json()) as UiConfig;
      } catch {
        return {};
      }
    },
    staleTime: Infinity,
  });
}

export function defaultDataPlaneBase(cfg: UiConfig | undefined): string {
  if (cfg?.dataPlaneUrl) return cfg.dataPlaneUrl;
  const scheme = cfg?.dataPlaneScheme ?? window.location.protocol.replace(':', '');
  const port = cfg?.dataPlanePort ?? 8080;
  return `${scheme}://${window.location.hostname}:${port}`;
}

export function storedDataPlaneBase(): string | null {
  try {
    return window.localStorage.getItem(BASE_KEY);
  } catch {
    return null;
  }
}

export function storeDataPlaneBase(v: string): void {
  try {
    window.localStorage.setItem(BASE_KEY, v);
  } catch {
    /* ignore */
  }
}
