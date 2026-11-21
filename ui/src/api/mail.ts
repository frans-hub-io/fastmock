import { useQuery } from '@tanstack/react-query';
import { api, buildQuery, errorFromResponse, rawFetch } from './http';
import { useVersion } from './features';
import type { components } from './schema';

type S = components['schemas'];

export type MailEntry = S['MailEntry'];
export type MailList = S['MailList'];
export type MailMessage = S['MailMessageView'];
export type MailAddress = S['MailAddress'];
export type MailAttachment = S['MailAttachment'];
export type MailHeader = S['MailHeader'];
export type MailEvent = S['MailEvent'];
export type MailLink = S['MailLink'];
export type TranscriptLine = S['TranscriptLine'];
export type MailboxView = S['MailboxView'];
export type MailChaosRule = S['MailChaosRule'];
export type MailChaosView = S['MailChaosView'];
export type ChaosStage = S['ChaosStage'];
export type ChaosCounters = S['ChaosCounters'];
export type ComposeRequest = S['ComposeRequest'];
export type ComposeAttachment = S['ComposeAttachment'];
export type MailStringMatcher = S['StringMatcher'];
export type ProviderExtras = S['ProviderExtras'];
export type MailMatch = S['MailMatch'];

const BASE = '/__admin/mail';

export function messagePath(id: string, sub = ''): string {
  return `${BASE}/messages/${encodeURIComponent(id)}${sub ? `/${sub}` : ''}`;
}

/** A rendition of a message as text (HTML body, text body, raw message); throws `ApiError` on failure. */
export async function fetchText(path: string, query?: Record<string, string | undefined>, accept = '*/*'): Promise<string> {
  const res = await rawFetch('GET', path, { query, accept });
  if (!res.ok) throw await errorFromResponse(res);
  return res.text();
}

/** A download (raw message, attachment) as a blob, with the authentication the API client uses. */
export async function fetchBlob(path: string): Promise<Blob> {
  const res = await rawFetch('GET', path, { accept: '*/*' });
  if (!res.ok) throw await errorFromResponse(res);
  return res.blob();
}

export const mailApi = {
  list: (q: { space?: string; query?: string; cursor?: string; limit?: number }) => api.get<MailList>(`${BASE}/messages`, { query: q }),
  get: (id: string) => api.get<MailMessage>(messagePath(id)),
  remove: (id: string) => api.del<undefined>(messagePath(id)),
  removeMatching: (q: { space?: string; query?: string }) => api.del<S['MailDeleted']>(`${BASE}/messages`, { query: q }),
  labels: (id: string, labels: string[]) => api.put<MailEntry>(messagePath(id, 'labels'), { labels }),
  transcript: (id: string) => api.get<S['MailTranscript']>(messagePath(id, 'transcript')),
  links: (id: string) => api.get<S['MailLinks']>(messagePath(id, 'links')),
  codes: (id: string, pattern?: string) => api.get<S['MailCodes']>(messagePath(id, 'codes'), { query: { pattern } }),
  inject: (body: ComposeRequest, space?: string | null) => api.post<unknown>(`${BASE}/messages`, body, { query: { space: space ?? undefined } }),
  injectRaw: (raw: string, q: { space?: string | null; mailFrom?: string; rcptTo?: string }) =>
    api.post<unknown>(`${BASE}/messages`, undefined, {
      rawBody: raw,
      contentType: 'message/rfc822',
      query: { space: q.space ?? undefined, mailFrom: q.mailFrom || undefined, rcptTo: q.rcptTo || undefined },
    }),
  mailboxes: (space?: string) => api.get<S['MailboxList']>(`${BASE}/mailboxes`, { query: { space } }),
  setPassword: (address: string, password: string, space?: string | null) =>
    api.put<undefined>(`${BASE}/mailboxes/${encodeURIComponent(address)}/password`, { password }, { query: { space: space ?? undefined } }),
  clearPassword: (address: string, space?: string | null) =>
    api.del<undefined>(`${BASE}/mailboxes/${encodeURIComponent(address)}/password`, { query: { space: space ?? undefined } }),
  chaos: (space?: string) => api.get<MailChaosView>(`${BASE}/chaos`, { query: { space } }),
  putChaos: (rules: MailChaosRule[], space?: string | null) => api.put<MailChaosView>(`${BASE}/chaos`, { rules }, { query: { space: space ?? undefined } }),
  clearChaos: (space?: string | null) => api.del<undefined>(`${BASE}/chaos`, { query: { space: space ?? undefined } }),
};

export function streamUrl(space?: string | null): string {
  return `${BASE}/stream${buildQuery({ space: space ?? undefined })}`;
}

export type MailFeature = 'loading' | 'on' | 'off';

/** Whether the server runs the mail module (`mail` in `GET /__admin/version` features). */
export function useMailFeature(): { state: MailFeature; features: string[] } {
  const v = useVersion();
  const features = v.data?.features ?? [];
  if (v.isLoading) return { state: 'loading', features };
  return { state: features.includes('mail') ? 'on' : 'off', features };
}

/** Optional mail listener ports the server may publish in `/ui/config.json`. */
export interface MailPorts {
  smtp?: number;
  imap?: number;
  pop3?: number;
}

export function useMailPorts(): MailPorts {
  const q = useQuery({
    queryKey: ['ui-config'],
    queryFn: async () => {
      try {
        const res = await fetch('/ui/config.json', { credentials: 'omit' });
        if (!res.ok) return {};
        return (await res.json()) as Record<string, unknown>;
      } catch {
        return {};
      }
    },
    staleTime: Infinity,
  });
  const cfg = (q.data ?? {});
  const num = (k: string) => (typeof cfg[k] === 'number' ? cfg[k] : undefined);
  return { smtp: num('mailSmtpPort'), imap: num('mailImapPort'), pop3: num('mailPop3Port') };
}
