export function downloadText(filename: string, text: string, type = 'application/json'): void {
  const blob = new Blob([text], { type });
  downloadBlob(filename, blob);
}

export function downloadBlob(filename: string, blob: Blob): void {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = filename;
  a.rel = 'noopener';
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}

export function readFileText(file: File, maxBytes = 32 * 1024 * 1024): Promise<string> {
  if (file.size > maxBytes) return Promise.reject(new Error(`${file.name} is larger than ${Math.round(maxBytes / 1024 / 1024)} MiB`));
  return file.text();
}

export function timestampedName(prefix: string, ext: string): string {
  const d = new Date().toISOString().replace(/[:.]/g, '-').slice(0, 19);
  return `${prefix}-${d}.${ext}`;
}
