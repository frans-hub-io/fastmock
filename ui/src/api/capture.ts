import { api } from './http';
import { useVersion } from './features';
import type { components } from './schema';

type S = components['schemas'];

export type MessageProtocol = S['MessageProtocol'];
export type CaptureSource = S['CaptureSource'];
export type SourceView = S['SourceView'];
export type SourceStatus = S['SourceStatus'];
export type SourceList = S['SourceList'];
export type MessageSummary = S['MessageSummary'];
export type MessageDetail = S['MessageDetail'];
export type DecodedBody = S['DecodedBody'];
export type MessageList = S['MessageList'];
export type MessageMatch = S['MessageMatch'];
export type FindResult = S['FindResult'];

const BASE = '/__admin/capture';

export function capturedPath(id: string, sub = ''): string {
  return `${BASE}/messages/${encodeURIComponent(id)}${sub ? `/${sub}` : ''}`;
}

export interface MessageQuery {
  space?: string;
  source?: string;
  protocol?: string;
  topic?: string;
  since?: string;
  until?: string;
  cursor?: string;
  limit?: number;
  order?: 'newest' | 'oldest';
}

export const captureApi = {
  sources: (space?: string) => api.get<SourceList>(`${BASE}/sources`, { query: { space } }),
  putSource: (id: string, body: CaptureSource, space?: string | null) => api.put<SourceView>(`${BASE}/sources/${encodeURIComponent(id)}`, body, { query: { space: space ?? undefined } }),
  removeSource: (id: string, purge: boolean) => api.del<undefined>(`${BASE}/sources/${encodeURIComponent(id)}`, { query: { purge: purge || undefined } }),
  list: (q: MessageQuery) => api.get<MessageList>(`${BASE}/messages`, { query: { ...q } }),
  find: (body: { match: MessageMatch; since?: string; limit?: number }, space?: string) => api.post<FindResult>(`${BASE}/messages/find`, body, { query: { space } }),
  get: (id: string) => api.get<MessageDetail>(capturedPath(id)),
  remove: (id: string) => api.del<undefined>(capturedPath(id)),
  removeAll: (q: { space?: string; source?: string }) => api.del<S['Deleted']>(`${BASE}/messages`, { query: q }),
};

export type CaptureFeature = 'loading' | 'on' | 'off';

/** Whether the server captures broker messages (`capture` in `GET /__admin/version` features). */
export function useCaptureFeature(): CaptureFeature {
  const v = useVersion();
  if (v.isLoading) return 'loading';
  return v.data?.features.includes('capture') ? 'on' : 'off';
}
