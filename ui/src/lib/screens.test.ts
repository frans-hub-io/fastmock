import { describe, expect, it } from 'vitest';
import { chaosEffects, chaosRuleIssues, fileKind, folderError, frequencyLabel, isHttpUrl, lognormalPercentile, methodMix, splitName, suggestScreens, tagError, utf8Bytes } from './screens';

describe('service summaries', () => {
  it('counts operations per method in the usual method order', () => {
    expect(methodMix([{ method: 'delete' }, { method: 'GET' }, { method: 'POST' }, { method: 'GET' }, { method: null }, { method: 'TRACE' }])).toEqual([
      ['GET', 2],
      ['POST', 1],
      ['DELETE', 1],
      ['TRACE', 1],
    ]);
  });
});

describe('chaos rules', () => {
  it('describes what a rule does', () => {
    expect(chaosEffects({ status: 503, delay: { fixedMs: 40 } }).map((e) => e.text)).toEqual(['503', '+40 ms']);
    expect(chaosEffects({ fault: 'CONNECTION_RESET', delay: { minMs: 200, maxMs: 900 } }).map((e) => e.text)).toEqual(['CONNECTION_RESET', '+200–900 ms']);
    expect(chaosEffects({})).toEqual([]);
  });

  it('explains why a new rule cannot be added', () => {
    const existing = [{ id: 'slow', probability: 0.5, delay: { fixedMs: 100 } }];
    expect(chaosRuleIssues({ id: '', probability: 0.1, status: 503 }, existing).id).toBe('Give the rule an id');
    expect(chaosRuleIssues({ id: 'slow', probability: 0.1, status: 503 }, existing).id).toBe('A rule with this id already exists');
    expect(chaosRuleIssues({ id: 'x'.repeat(65), probability: 0.1, status: 503 }, existing).id).toBe('At most 64 characters');
    expect(chaosRuleIssues({ id: 'a', probability: 0.1, status: 700 }, existing).status).toBe('A status between 100 and 599');
    expect(chaosRuleIssues({ id: 'a', probability: 0.1, status: 503, fault: 'EMPTY_RESPONSE' }, existing).status).toMatch(/fault/);
    expect(chaosRuleIssues({ id: 'a', probability: 0.1 }, existing).effect).toMatch(/does nothing/);
    expect(chaosRuleIssues({ id: 'a', probability: 0.1, status: 503 }, existing)).toEqual({});
    expect(chaosRuleIssues({ id: 'a', probability: 0.1, status: 503 }, existing, 1).limit).toBe('At most 1 rules');
  });
});

describe('form checks', () => {
  it('accepts only absolute http(s) URLs', () => {
    expect(isHttpUrl('https://api.example.com')).toBe(true);
    expect(isHttpUrl(' http://198.51.100.7:8080/base ')).toBe(true);
    expect(isHttpUrl('api.example.com')).toBe(false);
    expect(isHttpUrl('ftp://example.com')).toBe(false);
  });

  it('checks tags like the server', () => {
    expect(tagError('', false)).toBeUndefined();
    expect(tagError('', true)).toMatch(/Name a tag/);
    expect(tagError('rec-payments:v1/a_b.c', true)).toBeUndefined();
    expect(tagError('has space', true)).toMatch(/only/);
    expect(tagError('x'.repeat(129), true)).toBe('At most 128 characters');
  });

  it('checks upload folders like the server checks file names', () => {
    expect(folderError('')).toBeUndefined();
    expect(folderError('payments/v1/')).toBeUndefined();
    expect(folderError('/abs')).toMatch(/relative/);
    expect(folderError('a//b')).toMatch(/empty/);
    expect(folderError('a/../b')).toMatch(/\.\./);
    expect(folderError('a/./b')).toMatch(/\.\./);
    expect(folderError('naïve')).toMatch(/only/);
  });

  it('measures values in UTF-8 bytes', () => {
    expect(utf8Bytes('abc')).toBe(3);
    expect(utf8Bytes('ї')).toBe(2);
  });
});

describe('display helpers', () => {
  it('computes log-normal percentiles', () => {
    expect(lognormalPercentile(80, 0.4, 50)).toBe(80);
    expect(lognormalPercentile(80, 0.4, 95)).toBe(154);
    expect(lognormalPercentile(80, 0, 99)).toBe(80);
  });

  it('splits file names and tells their kind', () => {
    expect(splitName('orders/v1/list.json')).toEqual({ dir: 'orders/v1/', base: 'list.json' });
    expect(splitName('readme.txt')).toEqual({ dir: '', base: 'readme.txt' });
    expect(fileKind('a.bin', 'application/json')).toBe('json');
    expect(fileKind('a.wsdl', 'application/octet-stream')).toBe('xml');
    expect(fileKind('a.png', 'image/png')).toBe('image');
    expect(fileKind('a.csv', 'application/octet-stream')).toBe('text');
    expect(fileKind('a.bin', 'application/octet-stream')).toBe('binary');
  });

  it('labels publishing frequencies', () => {
    expect(frequencyLabel(null)).toBe('on trigger');
    expect(frequencyLabel(5000)).toBe('every 5 s');
    expect(frequencyLabel(120_000)).toBe('every 2 min');
    expect(frequencyLabel(250)).toBe('every 250 ms');
  });

  it('suggests screens that share a word with an unknown path, then the defaults', () => {
    const items = [
      { to: '/stubs', label: 'Stubs' },
      { to: '/requests', label: 'Request journal' },
      { to: '/near-misses', label: 'Near misses' },
      { to: '/chaos', label: 'Chaos' },
    ];
    expect(suggestScreens('/chaos-rules', items, ['/stubs'], 2).map((i) => i.to)).toEqual(['/chaos', '/stubs']);
    expect(suggestScreens('/stubz/old', items, ['/requests'], 3).map((i) => i.to)).toEqual(['/stubs', '/requests']);
    expect(suggestScreens('/x', items, ['/missing', '/near-misses']).map((i) => i.to)).toEqual(['/near-misses']);
  });
});
