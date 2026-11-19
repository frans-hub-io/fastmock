const KEY = 'mantine-color-scheme-value';

export function applyInitialColorScheme(): void {
  let stored: string | null;
  try {
    stored = window.localStorage.getItem(KEY);
  } catch {
    stored = null;
  }
  const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
  const scheme = stored === 'light' || stored === 'dark' ? stored : prefersDark ? 'dark' : 'light';
  document.documentElement.setAttribute('data-mantine-color-scheme', scheme);
}
