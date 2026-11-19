let cached: string | null | undefined;

export function styleNonce(): string | undefined {
  if (cached === undefined) {
    const v = document.querySelector('meta[name="fastmock-style-nonce"]')?.getAttribute('content') ?? '';
    cached = v && !v.startsWith('__') ? v : null;
  }
  return cached ?? undefined;
}
