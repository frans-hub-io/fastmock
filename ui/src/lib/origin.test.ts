import { describe, expect, it } from 'vitest';
import { defaultDataPlaneBase } from '../api/uiConfig';
import { sameOrigin } from './origin';

describe('sameOrigin', () => {
  const base = 'https://ui.example.com/ui/api-docs';

  it('accepts relative and same-origin absolute URLs', () => {
    expect(sameOrigin('/__admin/stubs', base)).toBe(true);
    expect(sameOrigin('__admin/stubs', base)).toBe(true);
    expect(sameOrigin('https://ui.example.com/__admin/stubs', base)).toBe(true);
    expect(sameOrigin('https://UI.example.com:443/x', base)).toBe(true);
  });

  it('rejects other origins that a prefix test would accept', () => {
    expect(sameOrigin('//evil.example/__admin/stubs', base)).toBe(false);
    expect(sameOrigin('https://ui.example.com.evil.net/__admin', base)).toBe(false);
    expect(sameOrigin('https://ui.example.com@evil.example/', base)).toBe(false);
    expect(sameOrigin('http://ui.example.com/__admin', base)).toBe(false);
    expect(sameOrigin('https://ui.example.com:8443/__admin', base)).toBe(false);
    expect(sameOrigin('javascript:alert(1)', base)).toBe(false);
    expect(sameOrigin('http://[bad', base)).toBe(false);
  });
});

describe('defaultDataPlaneBase', () => {
  it('prefers the configured data-plane URL', () => {
    expect(defaultDataPlaneBase({ dataPlaneUrl: 'https://mock.example.com', dataPlanePort: 8080 })).toBe('https://mock.example.com');
    expect(defaultDataPlaneBase({ dataPlanePort: 8081, dataPlaneScheme: 'http' })).toBe(`http://${window.location.hostname}:8081`);
  });
});
