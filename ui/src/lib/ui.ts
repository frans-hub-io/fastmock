import { useLocalStorage } from '@mantine/hooks';

export type Tone = 'success' | 'info' | 'warning' | 'danger' | 'neutral';

export function statusTone(status: number): Tone {
  if (status >= 500) return 'danger';
  if (status >= 400) return 'warning';
  if (status >= 300) return 'info';
  if (status >= 200) return 'success';
  return 'neutral';
}

export type Density = 'comfortable' | 'compact';

/** The row density of data tables, remembered in this browser. */
export function useDensity(): [Density, (d: Density) => void] {
  const [value, setValue] = useLocalStorage<Density>({ key: 'fastmock.density', defaultValue: 'comfortable', getInitialValueInEffect: false });
  return [value === 'compact' ? 'compact' : 'comfortable', setValue];
}

/** A value kept in this browser's localStorage; reads and writes never throw. */
export function readLocal(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writeLocal(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    /* storage unavailable: the preference lasts for this page only */
  }
}
