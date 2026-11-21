import { useEffect, useRef, useState } from 'react';
import { rawFetch } from '../api/http';
import type { MailEvent } from '../api/mail';
import { SseParser } from './mail';

export type StreamStatus = 'connecting' | 'live' | 'reconnecting' | 'off';

export type StreamEvent =
  | { type: 'message.received' | 'message.deleted'; event: MailEvent }
  /** The stream fell behind or reconnected: events may have been missed, list the messages again. */
  | { type: 'resync' };

export interface StreamOptions {
  space?: string | null;
  onEvent: (e: StreamEvent) => void;
  onStatus?: (s: StreamStatus) => void;
  /** Delay before reconnect attempt `n` (from 0); default 1 s doubling to 30 s with a little jitter. */
  backoff?: (attempt: number) => number;
}

export function defaultBackoff(attempt: number): number {
  const base = Math.min(30_000, 1000 * 2 ** Math.min(attempt, 5));
  return base + Math.floor(Math.random() * 250);
}

/**
 * Follows `GET /__admin/mail/stream`. `EventSource` cannot send the bearer header, so this reads the event
 * stream through `fetch` with the API client's authentication. Reconnects with backoff after errors and ends,
 * and reports `resync` after a reconnect (events may have been missed meanwhile) and when the server says so.
 * Stops for good on 401, 403 and 404. Returns a function that closes the stream.
 */
export function connectMailStream(opts: StreamOptions): () => void {
  const ctl = new AbortController();
  const backoff = opts.backoff ?? defaultBackoff;
  const st = { closed: false };
  const isClosed = () => st.closed;
  let timer: ReturnType<typeof setTimeout> | undefined;

  const status = (s: StreamStatus) => {
    if (!isClosed()) opts.onStatus?.(s);
  };

  const run = async (attempt: number, everConnected: boolean): Promise<void> => {
    if (isClosed()) return;
    status(attempt === 0 && !everConnected ? 'connecting' : 'reconnecting');
    let connected = everConnected;
    let healthy = false;
    try {
      const res = await rawFetch('GET', '/__admin/mail/stream', {
        query: { space: opts.space ?? undefined },
        accept: 'text/event-stream',
        signal: ctl.signal,
      });
      if (res.status === 401 || res.status === 403 || res.status === 404) {
        status('off');
        return;
      }
      if (!res.ok || !res.body) throw new Error(`stream answered ${String(res.status)}`);
      const reader = res.body.getReader();
      const decoder = new TextDecoder();
      const parser = new SseParser();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        for (const m of parser.push(decoder.decode(value, { stream: true }))) {
          if (isClosed()) return;
          if (m.comment !== undefined) {
            if (m.comment === 'ready') {
              healthy = true;
              status('live');
              if (connected) opts.onEvent({ type: 'resync' });
              connected = true;
            }
            continue;
          }
          if (m.event === 'resync') {
            opts.onEvent({ type: 'resync' });
            continue;
          }
          if (m.event === 'message.received' || m.event === 'message.deleted') {
            try {
              opts.onEvent({ type: m.event, event: JSON.parse(m.data) as MailEvent });
            } catch {
              opts.onEvent({ type: 'resync' });
            }
          }
        }
      }
    } catch {
      if (isClosed()) return;
    }
    if (isClosed()) return;
    const next = healthy ? 0 : attempt + 1;
    status('reconnecting');
    timer = setTimeout(() => void run(next, connected), backoff(next));
  };

  void run(0, false);
  return () => {
    st.closed = true;
    if (timer) clearTimeout(timer);
    ctl.abort();
  };
}

/** Follows the mail stream of a space while `enabled`; the latest `onEvent` is called for each event. */
export function useMailStream(space: string | null | undefined, enabled: boolean, onEvent: (e: StreamEvent) => void): StreamStatus {
  const [status, setStatus] = useState<StreamStatus>('connecting');
  const handler = useRef(onEvent);
  useEffect(() => {
    handler.current = onEvent;
  });
  useEffect(() => {
    if (!enabled) {
      setStatus('off');
      return undefined;
    }
    return connectMailStream({ space, onEvent: (e) => handler.current(e), onStatus: setStatus });
  }, [space, enabled]);
  return status;
}
