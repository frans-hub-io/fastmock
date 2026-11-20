import { describe, expect, it } from 'vitest';
import { pathKindOf, pathParts } from './path';
import { bucketize, compact, formatMs, hitsByStub, latency, niceTicks, quantile, statusClasses } from './traffic';
import { statusTone } from './ui';

describe('path display', () => {
  it('splits template parameters and the query', () => {
    expect(pathParts('/orders/{id}/items/{item...}?x=1', 'template')).toEqual([
      { text: '/orders/' },
      { text: '{id}', param: true },
      { text: '/items/' },
      { text: '{item...}', param: true },
      { text: '?x=1', query: true },
    ]);
  });

  it('keeps regular expressions whole', () => {
    expect(pathParts('/api/{2}/.*', 'regex')).toEqual([{ text: '/api/{2}/.*' }]);
  });

  it('maps URL matcher kinds', () => {
    expect(pathKindOf('urlPathTemplate')).toBe('template');
    expect(pathKindOf('urlPattern')).toBe('regex');
    expect(pathKindOf('urlPath')).toBe('exact');
    expect(pathKindOf('any')).toBe('any');
  });
});

describe('traffic summaries', () => {
  const now = Date.parse('2026-11-20T12:00:00Z');
  const at = (minAgo: number) => new Date(now - minAgo * 60_000).toISOString();

  it('counts matched and unmatched requests per bucket, oldest first', () => {
    const b = bucketize(
      [
        { at: at(59), matched: true },
        { at: at(1), matched: true },
        { at: at(1), matched: false },
        { at: at(61), matched: true },
      ],
      now,
      60 * 60_000,
      30,
    );
    expect(b).toHaveLength(30);
    expect(b[0]).toMatchObject({ matched: 1, unmatched: 0 });
    expect(b[29]).toMatchObject({ matched: 1, unmatched: 1 });
    expect(b.reduce((s, x) => s + x.matched + x.unmatched, 0)).toBe(3);
  });

  it('computes nearest-rank quantiles and the latency summary', () => {
    expect(quantile([], 0.5)).toBeUndefined();
    expect(quantile([5, 1, 3], 0.5)).toBe(3);
    expect(latency([{ durationMs: 1 }, { durationMs: 10 }, { durationMs: 100 }])).toEqual({ p50: 10, p95: 100, p99: 100, max: 100, count: 3 });
  });

  it('groups status classes and ranks stubs by hits', () => {
    expect(statusClasses([{ status: 200 }, { status: 204 }, { status: 404 }, { status: 503 }, { status: 101 }])).toEqual({ '2xx': 2, '3xx': 0, '4xx': 1, '5xx': 1, other: 1 });
    const hits = hitsByStub([
      { stubId: 'a', at: at(5), matched: true },
      { stubId: 'b', at: at(4), matched: true },
      { stubId: 'b', at: at(2), matched: true },
      { stubId: null, at: at(1), matched: false },
    ]);
    expect(hits.map((h) => [h.stubId, h.hits])).toEqual([
      ['b', 2],
      ['a', 1],
    ]);
    expect(hits[0]?.lastAt).toBe(at(2));
  });

  it('formats numbers, durations, ticks and status tones', () => {
    expect(compact(1284)).toBe('1,284');
    expect(compact(12_900)).toBe('12.9K');
    expect(compact(4_200_000)).toBe('4.2M');
    expect(formatMs(undefined)).toBe('—');
    expect(formatMs(12.4)).toBe('12 ms');
    expect(formatMs(1500)).toBe('1.50 s');
    expect(niceTicks(87)).toEqual([0, 50, 100]);
    expect(niceTicks(0)).toEqual([0, 1]);
    expect(statusTone(201)).toBe('success');
    expect(statusTone(302)).toBe('info');
    expect(statusTone(404)).toBe('warning');
    expect(statusTone(500)).toBe('danger');
  });
});
