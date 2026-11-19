export function sameOrigin(url: string, base: string): boolean {
  try {
    const own = new URL(base).origin;
    return own !== 'null' && new URL(url, base).origin === own;
  } catch {
    return false;
  }
}
