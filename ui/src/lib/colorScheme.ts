const KEY = 'mantine-color-scheme-value';
const MANTINE_ATTR = 'data-mantine-color-scheme';

export function applyInitialColorScheme(): void {
  let stored: string | null;
  try {
    stored = window.localStorage.getItem(KEY);
  } catch {
    stored = null;
  }
  const prefersDark = window.matchMedia('(prefers-color-scheme: dark)').matches;
  const scheme = stored === 'light' || stored === 'dark' ? stored : prefersDark ? 'dark' : 'light';
  document.documentElement.setAttribute(MANTINE_ATTR, scheme);
  document.documentElement.setAttribute('data-theme', scheme);
}

/** Keeps `data-theme` (brand tokens) equal to the color scheme Mantine writes on the same element. */
export function syncThemeAttribute(root: HTMLElement = document.documentElement): () => void {
  const apply = () => root.setAttribute('data-theme', root.getAttribute(MANTINE_ATTR) === 'dark' ? 'dark' : 'light');
  apply();
  const observer = new MutationObserver(apply);
  observer.observe(root, { attributes: true, attributeFilter: [MANTINE_ATTR] });
  return () => observer.disconnect();
}
