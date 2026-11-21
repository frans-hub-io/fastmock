/** The public FastMock documentation; links open in a new tab and load nothing into this page. */
export const DOCS_BASE = 'https://fastmock.franshub.io';

export function docsUrl(page: string): string {
  return `${DOCS_BASE}/${page.replace(/^\/+|\/+$/g, '')}/`;
}
